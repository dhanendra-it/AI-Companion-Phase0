//! Pure input logic: no Windows calls in here, so it is unit-tested on any OS.
//! The Windows hooks (hook.rs) feed raw key/mouse events in and act on the answer.

use super::proto::{self, Out};

/// Right Ctrl + Right Shift.
pub const DEFAULT_HOTKEY: [u32; 2] = [0xA3, 0xA1];

#[derive(Debug, PartialEq)]
pub enum KeyAction {
    /// Leave the key alone (laptop mode).
    Pass,
    /// Eat the key, send nothing.
    Swallow,
    /// The switching chord was pressed.
    Toggle,
    /// Eat the key and send this to the tablet.
    Send(Out),
}

/// Windows virtual key -> Android KEYCODE_* for keys that are not text.
fn special(vk: u32) -> Option<i32> {
    Some(match vk {
        0x0D => 66,  // Enter
        0x08 => 67,  // Backspace (KEYCODE_DEL)
        0x09 => 61,  // Tab
        0x1B => 111, // Escape
        0x2E => 112, // Delete (FORWARD_DEL)
        0x25 => 21,  // Left
        0x26 => 19,  // Up
        0x27 => 22,  // Right
        0x28 => 20,  // Down
        0x24 => 122, // Home
        0x23 => 123, // End
        0x21 => 92,  // PageUp
        0x22 => 93,  // PageDown
        0x2D => 124, // Insert
        _ => return None,
    })
}

fn is_modifier_or_lock(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0x5D | 0x90 | 0x91) || (0xA0..=0xA5).contains(&vk)
}

/// Some keyboard drivers report sided modifiers as generic VK_SHIFT/VK_CONTROL values.
/// Recover the right-side identity from the native scan code / extended-key flag.
fn normalize_vk(vk: u32, scan: u32, extended: bool) -> u32 {
    match vk {
        0x10 => if scan == 0x36 { 0xA1 } else { 0xA0 }, // VK_SHIFT -> left/right Shift
        0x11 => if extended { 0xA3 } else { 0xA2 },   // VK_CONTROL -> left/right Ctrl
        0x12 => if extended { 0xA5 } else { 0xA4 },   // VK_MENU -> left/right Alt
        _ => vk,
    }
}

fn letter_digit(vk: u32) -> Option<i32> {
    match vk {
        0x41..=0x5A => Some(29 + (vk as i32 - 0x41)), // A..Z
        0x30..=0x39 => Some(7 + (vk as i32 - 0x30)),  // 0..9
        0x20 => Some(62),                              // Space
        _ => None,
    }
}

pub struct KeyTracker {
    held: [bool; 256],
    swallowed_ups: [bool; 256],
    hotkey_keys: [bool; 256],
    caps: bool,
    caps_down: bool,
    hotkey: Vec<u32>,
}

impl KeyTracker {
    pub fn new(hotkey: &[u32]) -> Self {
        KeyTracker {
            held: [false; 256],
            swallowed_ups: [false; 256],
            hotkey_keys: [false; 256],
            caps: false,
            caps_down: false,
            hotkey: hotkey.to_vec(),
        }
    }

    /// Called when tablet mode starts, with the real Caps Lock state.
    pub fn set_caps(&mut self, on: bool) {
        self.caps = on;
    }

    /// `to_text(vk, scan, shift, caps)` turns a key into typed characters using the active layout.
    /// Key-up events from the low-level hook reset chord state. Do not query GetAsyncKeyState:
    /// tablet-mode key downs are swallowed, so Windows may report a held chord key as up.
    pub fn on_key(
        &mut self,
        vk: u32,
        scan: u32,
        extended: bool,
        down: bool,
        tablet: bool,
        to_text: &mut dyn FnMut(u32, u32, bool, bool) -> Option<String>,
    ) -> KeyAction {
        let vk = normalize_vk(vk, scan, extended);
        if vk > 255 {
            return KeyAction::Pass;
        }
        let i = vk as usize;

        if down {
            self.held[i] = true;
            if self.hotkey_keys[i] {
                return KeyAction::Swallow; // auto-repeat of a chord key
            }
            if self.hotkey_matches(vk) {
                self.hotkey_keys[i] = true;
                self.swallowed_ups[i] = true;
                return KeyAction::Toggle;
            }
            if !tablet {
                return KeyAction::Pass;
            }
            if (0xA6..=0xB7).contains(&vk) {
                return KeyAction::Pass; // volume / media / browser keys stay with Windows
            }
            self.swallowed_ups[i] = true;
            if vk == 0x14 {
                if !self.caps_down {
                    self.caps_down = true;
                    self.caps = !self.caps;
                }
                return KeyAction::Swallow;
            }
            return self.forward(vk, scan, to_text);
        }

        self.held[i] = false;
        self.hotkey_keys[i] = false;
        if vk == 0x14 {
            self.caps_down = false;
        }
        if std::mem::replace(&mut self.swallowed_ups[i], false) {
            KeyAction::Swallow
        } else {
            KeyAction::Pass
        }
    }

    fn hotkey_matches(&mut self, vk: u32) -> bool {
        if self.hotkey.is_empty() || !self.hotkey.contains(&vk) {
            return false;
        }
        for k in self.hotkey.clone() {
            if k == vk {
                continue;
            }
            if !self.held[k as usize] {
                return false;
            }
        }
        true
    }

    fn forward(
        &mut self,
        vk: u32,
        scan: u32,
        to_text: &mut dyn FnMut(u32, u32, bool, bool) -> Option<String>,
    ) -> KeyAction {
        if is_modifier_or_lock(vk) {
            return KeyAction::Swallow; // modifiers are never sent on their own
        }
        let h = |k: usize| self.held[k];
        let shift = h(0xA0) || h(0xA1);
        let ctrl = h(0xA2) || h(0xA3);
        let alt = h(0xA4) || h(0xA5);
        let meta = (if shift { 0x41 } else { 0 }) | (if ctrl { 0x3000 } else { 0 }) | (if alt { 0x12 } else { 0 });

        let out = if let Some(code) = special(vk) {
            Some(Out::key(code, meta))
        } else if (0x70..=0x7B).contains(&vk) {
            Some(Out::key(131 + (vk as i32 - 0x70), meta)) // F1..F12
        } else if ctrl || alt {
            letter_digit(vk).map(|code| Out::key(code, meta)) // shortcuts, e.g. Ctrl+C
        } else {
            to_text(vk, scan, shift, self.caps).filter(|t| !t.is_empty()).map(|t| Out::text(&t))
        };
        match out {
            Some(o) => KeyAction::Send(o),
            None => KeyAction::Swallow,
        }
    }
}

// Window messages seen by the low-level mouse hook.
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;

/// Turns low-level mouse events into pointer packets.
///
/// In tablet mode the cursor is parked at `anchor` (screen centre) and every mouse event is
/// swallowed, so each move event arrives as `anchor + movement`. The movement is the
/// difference to the anchor. Using the position (not raw input) means touchpads work too.
pub struct MouseTracker {
    anchor: (i32, i32),
    buttons: u8,
    wheel_acc: i32,
}

impl MouseTracker {
    pub fn new() -> Self {
        MouseTracker { anchor: (0, 0), buttons: 0, wheel_acc: 0 }
    }

    pub fn reset(&mut self, anchor: (i32, i32)) {
        self.anchor = anchor;
        self.buttons = 0;
        self.wheel_acc = 0;
    }

    pub fn anchor(&self) -> (i32, i32) {
        self.anchor
    }

    /// `data` is MSLLHOOKSTRUCT.mouseData.
    pub fn on_event(&mut self, msg: u32, x: i32, y: i32, data: u32) -> Option<Out> {
        let set = |b: &mut u8, bit: u8, on: bool| {
            if on { *b |= bit } else { *b &= !bit }
        };
        match msg {
            WM_MOUSEMOVE => {
                let (dx, dy) = (x - self.anchor.0, y - self.anchor.1);
                if dx == 0 && dy == 0 {
                    return None;
                }
                return Some(Out::Mouse { dx, dy, buttons: self.buttons, wheel: 0 });
            }
            WM_MOUSEWHEEL => {
                self.wheel_acc += ((data >> 16) as u16 as i16) as i32;
                let notches = self.wheel_acc / 120;
                if notches == 0 {
                    return None;
                }
                self.wheel_acc -= notches * 120;
                return Some(Out::Mouse { dx: 0, dy: 0, buttons: self.buttons, wheel: notches });
            }
            WM_LBUTTONDOWN => set(&mut self.buttons, proto::BTN_LEFT, true),
            WM_LBUTTONUP => set(&mut self.buttons, proto::BTN_LEFT, false),
            WM_RBUTTONDOWN => set(&mut self.buttons, proto::BTN_RIGHT, true),
            WM_RBUTTONUP => set(&mut self.buttons, proto::BTN_RIGHT, false),
            WM_MBUTTONDOWN => set(&mut self.buttons, proto::BTN_MIDDLE, true),
            WM_MBUTTONUP => set(&mut self.buttons, proto::BTN_MIDDLE, false),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let bit = match (data >> 16) & 0xFFFF {
                    1 => proto::BTN_BACK,
                    2 => proto::BTN_FORWARD,
                    _ => return None,
                };
                set(&mut self.buttons, bit, msg == WM_XBUTTONDOWN);
            }
            _ => return None,
        }
        Some(Out::Mouse { dx: 0, dy: 0, buttons: self.buttons, wheel: 0 })
    }

    /// Button state to send when tablet mode ends, so nothing stays pressed on the tablet.
    pub fn release_all(&mut self) -> Option<Out> {
        if self.buttons == 0 {
            return None;
        }
        self.buttons = 0;
        Some(Out::Mouse { dx: 0, dy: 0, buttons: 0, wheel: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RCTRL: u32 = 0xA3;
    const RSHIFT: u32 = 0xA1;
    const LSHIFT: u32 = 0xA0;
    const LCTRL: u32 = 0xA2;

    fn tracker() -> KeyTracker {
        KeyTracker::new(&DEFAULT_HOTKEY)
    }

    /// Feeds one key event with a fake layout: letters type themselves (lower/upper by shift/caps).
    fn key(t: &mut KeyTracker, vk: u32, down: bool, tablet: bool) -> KeyAction {
        let mut to_text = |vk: u32, _scan: u32, shift: bool, caps: bool| {
            if (0x41..=0x5A).contains(&vk) {
                let c = vk as u8 as char;
                Some(if shift ^ caps { c.to_string() } else { c.to_ascii_lowercase().to_string() })
            } else {
                None
            }
        };
        t.on_key(vk, 0, false, down, tablet, &mut to_text)
    }

    #[test]
    fn laptop_mode_passes_everything() {
        let mut t = tracker();
        assert_eq!(key(&mut t, 0x41, true, false), KeyAction::Pass);
        assert_eq!(key(&mut t, 0x41, false, false), KeyAction::Pass);
    }

    #[test]
    fn chord_toggles_and_only_its_last_key_up_is_swallowed() {
        let mut t = tracker();
        assert_eq!(key(&mut t, RCTRL, true, false), KeyAction::Pass); // windows saw Right Ctrl go down
        assert_eq!(key(&mut t, RSHIFT, true, false), KeyAction::Toggle);
        assert_eq!(key(&mut t, RSHIFT, true, true), KeyAction::Swallow); // auto-repeat
        assert_eq!(key(&mut t, RSHIFT, false, true), KeyAction::Swallow);
        assert_eq!(key(&mut t, RCTRL, false, true), KeyAction::Pass);
    }

    #[test]
    fn chord_returns_from_tablet_mode_even_when_swallowed_ctrl_is_not_visible_to_windows() {
        let mut t = KeyTracker::new(&DEFAULT_HOTKEY);
        let mut none = |_: u32, _: u32, _: bool, _: bool| None;
        assert_eq!(t.on_key(RCTRL, 0, true, true, true, &mut none), KeyAction::Swallow);
        assert_eq!(t.on_key(RSHIFT, 0, false, true, true, &mut none), KeyAction::Toggle);
    }

    #[test]
    fn chord_works_again_from_tablet_mode() {
        let mut t = tracker();
        assert_eq!(key(&mut t, RCTRL, true, true), KeyAction::Swallow);
        assert_eq!(key(&mut t, RSHIFT, true, true), KeyAction::Toggle);
        assert_eq!(key(&mut t, RSHIFT, false, false), KeyAction::Swallow);
        assert_eq!(key(&mut t, RCTRL, false, false), KeyAction::Swallow);
    }

    #[test]
    fn key_up_resets_partial_chord_state() {
        let mut t = tracker();
        assert_eq!(key(&mut t, RCTRL, true, false), KeyAction::Pass);
        assert_eq!(key(&mut t, RCTRL, false, false), KeyAction::Pass);
        assert_eq!(key(&mut t, RSHIFT, true, false), KeyAction::Pass);
    }

    #[test]
    fn typing_and_shift_and_caps() {
        let mut t = tracker();
        assert_eq!(key(&mut t, 0x41, true, true), KeyAction::Send(Out::text("a")));
        key(&mut t, LSHIFT, true, true);
        assert_eq!(key(&mut t, 0x41, true, true), KeyAction::Send(Out::text("A")));
        key(&mut t, LSHIFT, false, true);
        key(&mut t, 0x14, true, true);
        key(&mut t, 0x14, false, true);
        assert_eq!(key(&mut t, 0x41, true, true), KeyAction::Send(Out::text("A")));
        // the key-up of a swallowed key is swallowed too
        assert_eq!(key(&mut t, 0x41, false, true), KeyAction::Swallow);
    }

    #[test]
    fn special_keys_and_shortcuts() {
        let mut t = tracker();
        assert_eq!(key(&mut t, 0x08, true, true), KeyAction::Send(Out::key(67, 0)));
        assert_eq!(key(&mut t, 0x1B, true, true), KeyAction::Send(Out::key(111, 0)));
        assert_eq!(key(&mut t, 0x70, true, true), KeyAction::Send(Out::key(131, 0)));
        key(&mut t, LCTRL, true, true);
        assert_eq!(key(&mut t, 0x43, true, true), KeyAction::Send(Out::key(31, 0x3000))); // Ctrl+C
        key(&mut t, LSHIFT, true, true);
        assert_eq!(key(&mut t, 0x25, true, true), KeyAction::Send(Out::key(21, 0x3041))); // Ctrl+Shift+Left
    }

    #[test]
    fn modifiers_alone_and_media_keys() {
        let mut t = tracker();
        assert_eq!(key(&mut t, LSHIFT, true, true), KeyAction::Swallow);
        assert_eq!(key(&mut t, 0xAF, true, true), KeyAction::Pass); // volume up stays on the laptop
        assert_eq!(key(&mut t, 0xAF, false, true), KeyAction::Pass);
    }

    fn mouse() -> MouseTracker {
        let mut m = MouseTracker::new();
        m.reset((100, 100));
        m
    }

    #[test]
    fn move_is_the_offset_from_the_anchor() {
        let mut m = mouse();
        assert_eq!(m.on_event(WM_MOUSEMOVE, 100, 100, 0), None);
        assert_eq!(m.on_event(WM_MOUSEMOVE, 103, 98, 0), Some(Out::Mouse { dx: 3, dy: -2, buttons: 0, wheel: 0 }));
    }

    #[test]
    fn buttons_are_tracked_and_released() {
        let mut m = mouse();
        assert_eq!(m.on_event(WM_LBUTTONDOWN, 100, 100, 0), Some(Out::Mouse { dx: 0, dy: 0, buttons: 1, wheel: 0 }));
        assert_eq!(m.on_event(WM_MOUSEMOVE, 101, 100, 0), Some(Out::Mouse { dx: 1, dy: 0, buttons: 1, wheel: 0 }));
        assert_eq!(m.on_event(WM_XBUTTONDOWN, 100, 100, 1 << 16), Some(Out::Mouse { dx: 0, dy: 0, buttons: 9, wheel: 0 }));
        assert_eq!(m.release_all(), Some(Out::Mouse { dx: 0, dy: 0, buttons: 0, wheel: 0 }));
        assert_eq!(m.release_all(), None);
    }

    #[test]
    fn wheel_accumulates_small_touchpad_steps() {
        let mut m = mouse();
        let step = |d: i16| (d as u16 as u32) << 16;
        assert_eq!(m.on_event(WM_MOUSEWHEEL, 0, 0, step(60)), None);
        assert_eq!(m.on_event(WM_MOUSEWHEEL, 0, 0, step(60)), Some(Out::Mouse { dx: 0, dy: 0, buttons: 0, wheel: 1 }));
        assert_eq!(m.on_event(WM_MOUSEWHEEL, 0, 0, step(-240)), Some(Out::Mouse { dx: 0, dy: 0, buttons: 0, wheel: -2 }));
    }
}
