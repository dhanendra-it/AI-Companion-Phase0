//! Windows side of the bridge: global low-level keyboard and mouse hooks.
//!
//! Laptop mode: nothing is touched, the hooks only watch for the switching shortcut.
//! Tablet mode: keys and mouse events are swallowed and sent to the tablet; the cursor is parked
//! at the screen centre so mouse movement can be read as "offset from the centre".
//!
//! The hooks live on their own thread with its own message loop, so a busy WebView2 or an AI
//! reply on Aalu's main thread can never delay the keyboard. The callbacks only flip flags and
//! queue packets (no blocking work), because Windows removes slow low-level hooks.
//! If Aalu dies, Windows removes the hooks and the laptop keyboard works again.

use super::{
    input::{KeyAction, KeyTracker, MouseTracker, DEFAULT_HOTKEY},
    lock, Shared,
};
use std::{
    panic::catch_unwind,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{GetKeyState, GetKeyboardLayout, ToUnicodeEx},
            WindowsAndMessaging::{
                CallNextHookEx, GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics,
                GetWindowThreadProcessId, SetCursorPos, SetWindowsHookExW, HHOOK, KBDLLHOOKSTRUCT,
                LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN,
                WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_MOUSEMOVE, WM_SYSKEYDOWN,
            },
        },
    },
};

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static KEYS: OnceLock<Mutex<KeyTracker>> = OnceLock::new();
static MOUSE: OnceLock<Mutex<MouseTracker>> = OnceLock::new();
/// Where the laptop cursor was when tablet mode started.
static SAVED_CURSOR: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static ALT_W_HELD: AtomicBool = AtomicBool::new(false);
static RIGHT_PRESS: Mutex<Option<Instant>> = Mutex::new(None);
static RIGHT_LONG_TRIGGERED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeRect { left: i32, top: i32, right: i32, bottom: i32 }
type MonitorCallback = unsafe extern "system" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, *mut NativeRect, isize) -> i32;

#[link(name = "gdi32")]
extern "system" {
    fn EnumDisplayMonitors(hdc: *mut std::ffi::c_void, clip: *const NativeRect, callback: Option<MonitorCallback>, data: isize) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn keybd_event(vk: u8, scan: u8, flags: u32, extra: usize);
    #[link_name = "mouse_event"]
    fn win_mouse_event(flags: u32, dx: u32, dy: u32, data: u32, extra: usize);
}

unsafe extern "system" fn collect_monitor(_: *mut std::ffi::c_void, _: *mut std::ffi::c_void, rect: *mut NativeRect, data: isize) -> i32 {
    if !rect.is_null() {
        let monitors = &mut *(data as *mut Vec<NativeRect>);
        let r = &*rect;
        monitors.push(*r);
    }
    1
}

fn cycle_monitor_cursor() {
    unsafe {
        let mut monitors = Vec::<NativeRect>::new();
        let _ = EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(collect_monitor), &mut monitors as *mut _ as isize);
        if monitors.len() < 2 { return; }
        let mut cursor = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut cursor).is_err() { return; }
        let current = monitors.iter().position(|r| {
            cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom
        }).unwrap_or(0);
        let next = monitors[(current + 1) % monitors.len()];
        let x = next.left + (next.right - next.left) / 2;
        let y = next.top + (next.bottom - next.top) / 2;
        let _ = SetCursorPos(x, y);
    }
}

fn launch_windows_voice_typing() {
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    unsafe {
        keybd_event(0x5B, 0, 0, 0); // Left Windows down
        keybd_event(b'H', 0, 0, 0);
        keybd_event(b'H', 0, KEYEVENTF_KEYUP, 0);
        keybd_event(0x5B, 0, KEYEVENTF_KEYUP, 0);
    }
}

fn start_right_hold_watcher() {
    let _ = thread::Builder::new().name("aalu-right-hold".into()).spawn(|| loop {
        thread::sleep(Duration::from_millis(20));
        let mut press = lock(&RIGHT_PRESS);
        if !RIGHT_LONG_TRIGGERED.load(Ordering::Relaxed)
            && press.map(|started| started.elapsed() >= Duration::from_secs(1)).unwrap_or(false)
        {
            RIGHT_LONG_TRIGGERED.store(true, Ordering::Relaxed);
            launch_windows_voice_typing();
        }
    });
}

pub fn start(shared: Arc<Shared>) {
    let _ = SHARED.set(shared.clone());
    let _ = KEYS.set(Mutex::new(KeyTracker::new(&DEFAULT_HOTKEY)));
    let _ = MOUSE.set(Mutex::new(MouseTracker::new()));
    start_right_hold_watcher();
    let hook_shared = shared.clone();
    let spawned = thread::Builder::new().name("deskbridge-hook".into()).spawn(move || unsafe { run(hook_shared) });
    if let Err(e) = spawned {
        eprintln!("DeskBridge: could not start the hook thread: {e}");
        shared.emit(super::Event::Refused(format!("DeskBridge shortcut could not start: {e}")));
    }
}

unsafe fn run(shared: Arc<Shared>) {
    let hinst = GetModuleHandleW(PCWSTR::null()).map(HINSTANCE::from).unwrap_or_default();
    if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0) {
        eprintln!("DeskBridge: could not install the keyboard hook: {e}");
        super::log(&format!("keyboard hook FAILED: {e}"));
        shared.emit(super::Event::Refused(format!("DeskBridge shortcut unavailable: {e}")));
        return;
    }
    if let Err(e) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0) {
        eprintln!("DeskBridge: could not install the mouse hook (keyboard still works): {e}");
        shared.emit(super::Event::Refused(format!("DeskBridge mouse hook unavailable: {e}")));
    }
    // Low-level hooks are called from this thread's message loop.
    super::log("hooks installed, message loop running");
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).0 > 0 {}
}

// ------------------------------------------------------------------ mode switch

/// Tablet mode is about to start: remember the cursor, park it at the screen centre, read Caps Lock.
pub fn enter() {
    unsafe {
        let centre = (GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2);
        let mut p = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut p).is_ok() {
            *lock(&SAVED_CURSOR) = Some((p.x, p.y));
        }
        let _ = SetCursorPos(centre.0, centre.1);
        if let Some(k) = KEYS.get() {
            lock(k).set_caps(GetKeyState(0x14) & 1 != 0);
        }
        if let Some(m) = MOUSE.get() {
            lock(m).reset(centre);
        }
    }
}

/// Tablet mode has ended: let go of any pressed mouse button on the tablet, restore the cursor.
pub fn leave() {
    let release = MOUSE.get().and_then(|m| lock(m).release_all());
    if let (Some(out), Some(sh)) = (release, SHARED.get()) {
        sh.send(out);
    }
    let saved = lock(&SAVED_CURSOR).take();
    if let Some((x, y)) = saved {
        unsafe {
            let _ = SetCursorPos(x, y);
        }
    }
}

// ------------------------------------------------------------------ keyboard

unsafe extern "system" fn keyboard_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 && catch_unwind(|| key_event(w, l)).unwrap_or(false) {
        return LRESULT(1); // swallow
    }
    CallNextHookEx(HHOOK::default(), code, w, l)
}

/// Returns true to swallow the key.
fn key_event(w: WPARAM, l: LPARAM) -> bool {
    let k = unsafe { *(l.0 as *const KBDLLHOOKSTRUCT) };
    if k.flags.0 & LLKHF_INJECTED.0 != 0 {
        return false; // synthetic key (another program, or us): never touch
    }
    let (Some(sh), Some(keys)) = (SHARED.get(), KEYS.get()) else { return false };
    let msg = w.0 as u32;
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    // Alt+W cycles the cursor through connected displays in normal laptop mode.
    if k.vkCode == 0x57 {
        if down && (unsafe { GetKeyState(0x12) as u16 } & 0x8000 != 0) {
            if !ALT_W_HELD.swap(true, Ordering::Relaxed) { cycle_monitor_cursor(); }
            return true;
        }
        if !down && ALT_W_HELD.swap(false, Ordering::Relaxed) { return true; }
    }
    let tablet = sh.tablet.load(Ordering::Relaxed);
    let extended = k.flags.0 & LLKHF_EXTENDED.0 != 0;
    if matches!(k.vkCode, 0x10..=0x12 | 0xA0..=0xA5) {
        super::log(&format!("modifier vk={:#x} scan={:#x} extended={extended} down={down} tablet={tablet}", k.vkCode, k.scanCode));
    }

    let action = lock(keys).on_key(k.vkCode, k.scanCode, extended, down, tablet, &mut to_text);
    match action {
        KeyAction::Pass => false,
        KeyAction::Swallow => true,
        KeyAction::Toggle => {
            sh.toggle();
            true
        }
        KeyAction::Send(out) => {
            sh.send(out);
            true
        }
    }
}

/// The characters a key types with the layout of the window that has focus.
fn to_text(vk: u32, scan: u32, shift: bool, caps: bool) -> Option<String> {
    unsafe {
        let mut state = [0u8; 256];
        if shift {
            state[0x10] = 0x80;
            state[0xA0] = 0x80;
        }
        if caps {
            state[0x14] = 1;
        }
        state[0x90] = 1; // numpad digits behave as if NumLock is on
        let thread_id = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let layout = GetKeyboardLayout(thread_id);
        let mut buf = [0u16; 8];
        // flag 4: do not disturb the dead-key state of the laptop's own typing
        let n = ToUnicodeEx(vk, scan, &state, &mut buf, 4, layout);
        if n <= 0 {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        if s.chars().any(char::is_control) {
            None
        } else {
            Some(s)
        }
    }
}

// ------------------------------------------------------------------ mouse

unsafe extern "system" fn mouse_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 && catch_unwind(|| mouse_event(w, l)).unwrap_or(false) {
        return LRESULT(1); // swallow
    }
    CallNextHookEx(HHOOK::default(), code, w, l)
}

/// Returns true to swallow the event.
fn mouse_event(w: WPARAM, l: LPARAM) -> bool {
    let Some(sh) = SHARED.get() else { return false };
    let m = unsafe { *(l.0 as *const MSLLHOOKSTRUCT) };
    let msg = w.0 as u32;

    // In laptop mode, reserve a one-second right-button hold for native Windows Voice Typing.
    // Swallow the physical down; replay a normal click only if the button is released early.
    if !sh.tablet.load(Ordering::Relaxed) && m.flags & LLMHF_INJECTED == 0 {
        const WM_RBUTTONDOWN: u32 = 0x0204;
        const WM_RBUTTONUP: u32 = 0x0205;
        if msg == WM_RBUTTONDOWN {
            *lock(&RIGHT_PRESS) = Some(Instant::now());
            RIGHT_LONG_TRIGGERED.store(false, Ordering::Relaxed);
            return true;
        }
        if msg == WM_RBUTTONUP {
            let was_pressed = lock(&RIGHT_PRESS).take().is_some();
            if was_pressed {
                if RIGHT_LONG_TRIGGERED.swap(false, Ordering::Relaxed) {
                    return true;
                }
                // A short press remains an ordinary right-click. Injected events bypass this hook.
                unsafe {
                    win_mouse_event(0x0008, 0, 0, 0, 0); // RIGHTDOWN
                    win_mouse_event(0x0010, 0, 0, 0, 0); // RIGHTUP
                }
                return true;
            }
        }
        return false;
    }
    if !sh.tablet.load(Ordering::Relaxed) { return false; }
    if m.flags & LLMHF_INJECTED != 0 {
        return false; // our own cursor parking, or another program
    }
    let Some(tracker) = MOUSE.get() else { return false };
    let (out, anchor) = {
        let mut t = lock(tracker);
        (t.on_event(msg, m.pt.x, m.pt.y, m.mouseData), t.anchor())
    };
    if let Some(out) = out {
        sh.send(out);
    }
    if msg == WM_MOUSEMOVE {
        // Normally the cursor never leaves the anchor because every move is swallowed.
        // If it did drift, put it back so the next offset is correct.
        unsafe {
            let mut p = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut p).is_ok() && (p.x, p.y) != anchor {
                let _ = SetCursorPos(anchor.0, anchor.1);
            }
        }
    }
    true
}
