//! Wire format PC -> tablet (big-endian), shared with the DeskBridge Android app.
//!
//!   'T' u16(len) utf8[len]                       commit text
//!   'K' i32(android keycode) i32(meta)           press + release one key
//!   'M' i16(dx) i16(dy) u8(buttons) i16(wheel)   pointer: relative move, button state, wheel notches
//!
//! The tablet sends one 0x01 "hello" byte when the connection opens and nothing after.
//! Button bits: 1 left, 2 right, 4 middle, 8 back, 16 forward. Wheel > 0 scrolls up.

pub const MAX_TEXT: usize = 1024;

pub const BTN_LEFT: u8 = 1;
pub const BTN_RIGHT: u8 = 2;
pub const BTN_MIDDLE: u8 = 4;
pub const BTN_BACK: u8 = 8;
pub const BTN_FORWARD: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// A finished packet ('T' or 'K').
    Bytes(Vec<u8>),
    /// Pointer packet. Kept unencoded so the writer can merge a burst of moves into one.
    Mouse { dx: i32, dy: i32, buttons: u8, wheel: i32 },
}

impl Out {
    pub fn text(s: &str) -> Out {
        let mut end = s.len().min(MAX_TEXT);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let data = &s.as_bytes()[..end];
        let mut p = Vec::with_capacity(3 + data.len());
        p.push(b'T');
        p.extend_from_slice(&(data.len() as u16).to_be_bytes());
        p.extend_from_slice(data);
        Out::Bytes(p)
    }

    pub fn key(android_keycode: i32, meta: i32) -> Out {
        let mut p = Vec::with_capacity(9);
        p.push(b'K');
        p.extend_from_slice(&android_keycode.to_be_bytes());
        p.extend_from_slice(&meta.to_be_bytes());
        Out::Bytes(p)
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Out::Bytes(b) => b.clone(),
            Out::Mouse { dx, dy, buttons, wheel } => {
                let c = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let mut p = Vec::with_capacity(8);
                p.push(b'M');
                p.extend_from_slice(&c(*dx).to_be_bytes());
                p.extend_from_slice(&c(*dy).to_be_bytes());
                p.push(*buttons);
                p.extend_from_slice(&c(*wheel).to_be_bytes());
                p
            }
        }
    }

    /// Folds `next` into `self` when both are pointer packets with the same button state.
    /// A button change is never merged, so a quick press+release can not be lost.
    pub fn merge(&mut self, next: &Out) -> bool {
        if let (
            Out::Mouse { dx, dy, buttons, wheel },
            Out::Mouse { dx: ndx, dy: ndy, buttons: nb, wheel: nw },
        ) = (&mut *self, next)
        {
            if *buttons == *nb {
                *dx += ndx;
                *dy += ndy;
                *wheel += nw;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_packet_layout() {
        assert_eq!(Out::text("hi").encode(), vec![b'T', 0, 2, b'h', b'i']);
    }

    #[test]
    fn text_is_cut_on_a_char_boundary() {
        let long = "é".repeat(MAX_TEXT); // 2 bytes each
        let Out::Bytes(p) = Out::text(&long) else { panic!() };
        let len = u16::from_be_bytes([p[1], p[2]]) as usize;
        assert!(len <= MAX_TEXT && len % 2 == 0);
        assert!(std::str::from_utf8(&p[3..]).is_ok());
    }

    #[test]
    fn key_packet_layout() {
        assert_eq!(Out::key(67, 0x3000).encode(), vec![b'K', 0, 0, 0, 67, 0, 0, 0x30, 0]);
    }

    #[test]
    fn mouse_packet_layout_and_clamp() {
        let p = Out::Mouse { dx: -2, dy: 5, buttons: BTN_LEFT | BTN_RIGHT, wheel: 1 }.encode();
        assert_eq!(p, vec![b'M', 0xFF, 0xFE, 0, 5, 3, 0, 1]);
        let p = Out::Mouse { dx: 100_000, dy: -100_000, buttons: 0, wheel: 0 }.encode();
        assert_eq!(&p[1..5], &[0x7F, 0xFF, 0x80, 0x00]);
        assert_eq!(p.len(), 8);
    }

    #[test]
    fn moves_merge_but_button_changes_do_not() {
        let mut a = Out::Mouse { dx: 1, dy: 2, buttons: 0, wheel: 0 };
        assert!(a.merge(&Out::Mouse { dx: 3, dy: 4, buttons: 0, wheel: 1 }));
        assert_eq!(a, Out::Mouse { dx: 4, dy: 6, buttons: 0, wheel: 1 });
        assert!(!a.merge(&Out::Mouse { dx: 0, dy: 0, buttons: BTN_LEFT, wheel: 0 }));
        assert!(!a.merge(&Out::key(1, 0)));
    }
}
