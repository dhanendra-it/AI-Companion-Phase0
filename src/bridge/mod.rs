//! DeskBridge inside Aalu: share the laptop keyboard and mouse with an Android tablet over USB.
//!
//! Press the switching shortcut (Right Ctrl + Right Shift) and every key and mouse move goes to the
//! tablet instead of the laptop; press it again to come back. If the cable is pulled the laptop
//! gets its keyboard and mouse back by itself. The tablet runs the DeskBridge Android app
//! (a keyboard plus a pointer service); this module only needs adb.exe.
//!
//! Threads: `deskbridge-hook` (global hooks, Windows only), `-link` (adb + connection state),
//! `-track` (adb track-devices), `-writer` (socket writes), `-reader` (detects a closed socket).
//! The hook thread never blocks: it only flips flags and queues packets.

mod adb;
mod input;
mod proto;

#[cfg(windows)]
mod hook;
/// The hooks are Windows only; elsewhere (unit tests on Linux) they do nothing.
#[cfg(not(windows))]
mod hook {
    use super::Shared;
    use std::sync::Arc;
    pub fn start(_: Arc<Shared>) {}
    pub fn enter() {}
    pub fn leave() {}
}

use proto::Out;
use std::{
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Condvar, Mutex, MutexGuard, TryLockError,
    },
    time::Duration,
};

/// What the app is told about. Sent from bridge threads; handle it on the UI thread.
#[derive(Debug, Clone)]
pub enum Event {
    /// Tablet mode switched on or off.
    Mode(bool),
    /// The shortcut was pressed but the tablet is not connected (human readable reason).
    Refused(String),
    /// Connection state changed.
    Link { connected: bool, message: String },
}

/// Diagnostic log at %LOCALAPPDATA%\Aalu\bridge.log (small, cleared when it grows past 256 KB).
/// Only bridge events are written: hook/adb/connection steps and the two shortcut keys.
/// Typed text and ordinary keys are never logged.
pub fn log(msg: &str) {
    use std::io::Write;
    let Some(base) = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("TEMP")) else { return };
    let dir = std::path::PathBuf::from(base).join("Aalu");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("bridge.log")) else { return };
    if f.metadata().map(|m| m.len()).unwrap_or(0) > 262_144 {
        let _ = f.set_len(0);
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = writeln!(f, "[{ms}] {msg}");
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Shared {
    /// Keys and mouse currently go to the tablet.
    pub tablet: AtomicBool,
    /// A live connection to the tablet keyboard app exists.
    pub connected: AtomicBool,
    /// Bumped on every connect and disconnect; lets threads notice a stale socket.
    pub conn_gen: AtomicU64,
    pub stream: Mutex<Option<TcpStream>>,
    tx: SyncSender<Out>,
    notify: Box<dyn Fn(Event) + Send + Sync>,
    last_msg: Mutex<String>,
    last_link: Mutex<Option<(bool, String)>>,
    mode_lock: Mutex<()>,
    wake: (Mutex<bool>, Condvar),
}

impl Shared {
    pub(crate) fn new(tx: SyncSender<Out>, notify: Box<dyn Fn(Event) + Send + Sync>) -> Self {
        Shared {
            tablet: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            conn_gen: AtomicU64::new(0),
            stream: Mutex::new(None),
            tx,
            notify,
            last_msg: Mutex::new("Tablet not connected.".into()),
            last_link: Mutex::new(None),
            mode_lock: Mutex::new(()),
            wake: (Mutex::new(false), Condvar::new()),
        }
    }

    fn emit(&self, e: Event) {
        (self.notify)(e);
    }

    /// Queues a packet. Never blocks. False when the tablet is not connected or the queue is full.
    pub(crate) fn send(&self, out: Out) -> bool {
        self.connected.load(Ordering::SeqCst) && self.tx.try_send(out).is_ok()
    }

    pub(crate) fn install_stream(&self, s: TcpStream) -> u64 {
        *lock(&self.stream) = Some(s);
        let gen = self.conn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.connected.store(true, Ordering::SeqCst);
        gen
    }

    pub(crate) fn link(&self, connected: bool, msg: &str) {
        *lock(&self.last_msg) = msg.to_string();
        let mut last = lock(&self.last_link);
        if last.as_ref().map(|(c, m)| (*c, m.as_str())) == Some((connected, msg)) {
            return;
        }
        *last = Some((connected, msg.to_string()));
        drop(last);
        log(&format!("link connected={connected}: {msg}"));
        self.emit(Event::Link { connected, message: msg.to_string() });
    }

    pub(crate) fn wake(&self) {
        *lock(&self.wake.0) = true;
        self.wake.1.notify_all();
    }

    /// Sleeps until `wake()` or the timeout, then clears the wake flag.
    pub(crate) fn wait_wake(&self, d: Duration) {
        let g = lock(&self.wake.0);
        let (mut g, _) = self.wake.1.wait_timeout_while(g, d, |woken| !*woken).unwrap_or_else(|e| e.into_inner());
        *g = false;
    }

    /// The shortcut. Called from the hook thread, which must never block: if another thread is
    /// in the middle of switching, this press is simply ignored.
    pub(crate) fn toggle(&self) {
        log(&format!(
            "shortcut pressed: tablet={} connected={}",
            self.tablet.load(Ordering::SeqCst),
            self.connected.load(Ordering::SeqCst)
        ));
        let _g = match self.mode_lock.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        if self.tablet.load(Ordering::SeqCst) {
            self.switch(false);
        } else if self.connected.load(Ordering::SeqCst) {
            self.switch(true);
        } else {
            let why = short_reason(&lock(&self.last_msg));
            self.emit(Event::Refused(why));
        }
    }

    pub(crate) fn set_tablet(&self, on: bool) {
        let _g = lock(&self.mode_lock);
        self.switch(on);
    }

    /// `mode_lock` must be held.
    fn switch(&self, on: bool) {
        if self.tablet.load(Ordering::SeqCst) == on || (on && !self.connected.load(Ordering::SeqCst)) {
            return;
        }
        if on {
            hook::enter(); // park the cursor, learn Caps Lock
            self.tablet.store(true, Ordering::SeqCst);
        } else {
            self.tablet.store(false, Ordering::SeqCst);
            hook::leave(); // release buttons on the tablet, put the cursor back
        }
        self.emit(Event::Mode(on));
    }
}

/// First sentence of a status message, short enough for the small robot window.
fn short_reason(msg: &str) -> String {
    let first = msg.split(". ").next().unwrap_or(msg).trim().trim_end_matches('.');
    let mut s: String = first.chars().take(70).collect();
    if first.chars().count() > 70 {
        s.push('\u{2026}');
    }
    s
}

/// Starts the bridge. `notify` is called from bridge threads.
pub fn start(notify: impl Fn(Event) + Send + Sync + 'static) {
    log("bridge start");
    let (tx, rx) = sync_channel(1024);
    let shared = Arc::new(Shared::new(tx, Box::new(notify)));
    adb::spawn(shared.clone(), rx);
    hook::start(shared);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn shared() -> (Arc<Shared>, mpsc::Receiver<Event>) {
        let (tx, _rx) = sync_channel(8);
        let (ev_tx, ev_rx) = mpsc::channel();
        let ev_tx = Mutex::new(ev_tx);
        let sh = Arc::new(Shared::new(tx, Box::new(move |e| {
            let _ = ev_tx.lock().unwrap().send(e);
        })));
        (sh, ev_rx)
    }

    #[test]
    fn shortcut_is_refused_while_the_tablet_is_not_connected() {
        let (sh, ev) = shared();
        sh.link(false, "No tablet found.");
        let _ = ev.try_recv(); // the Link event
        sh.toggle();
        match ev.try_recv().unwrap() {
            Event::Refused(why) => assert_eq!(why, "No tablet found"),
            e => panic!("{e:?}"),
        }
        assert!(!sh.tablet.load(Ordering::SeqCst));
    }

    #[test]
    fn shortcut_switches_both_ways_and_disconnect_returns_to_laptop() {
        let (sh, ev) = shared();
        sh.connected.store(true, Ordering::SeqCst);
        sh.toggle();
        assert!(sh.tablet.load(Ordering::SeqCst));
        assert!(matches!(ev.try_recv().unwrap(), Event::Mode(true)));
        sh.toggle();
        assert!(!sh.tablet.load(Ordering::SeqCst));
        assert!(matches!(ev.try_recv().unwrap(), Event::Mode(false)));

        sh.toggle();
        assert!(sh.tablet.load(Ordering::SeqCst));
        let _ = ev.try_recv();
        sh.teardown(); // cable pulled
        assert!(!sh.tablet.load(Ordering::SeqCst));
        assert!(!sh.connected.load(Ordering::SeqCst));
        assert!(matches!(ev.try_recv().unwrap(), Event::Mode(false)));
    }

    #[test]
    fn refusal_text_is_one_short_sentence() {
        assert_eq!(short_reason("No tablet found. Plug in the USB cable."), "No tablet found");
        assert_eq!(short_reason("Tablet not connected."), "Tablet not connected");
        let long = "x".repeat(200);
        assert_eq!(short_reason(&long).chars().count(), 71);
    }

    #[test]
    fn identical_link_messages_are_reported_once() {
        let (sh, ev) = shared();
        sh.link(false, "x");
        sh.link(false, "x");
        sh.link(true, "y");
        assert!(matches!(ev.try_recv().unwrap(), Event::Link { connected: false, .. }));
        assert!(matches!(ev.try_recv().unwrap(), Event::Link { connected: true, .. }));
        assert!(ev.try_recv().is_err());
    }

    #[test]
    fn wake_ends_the_wait_early() {
        let (sh, _ev) = shared();
        sh.wake();
        let t = std::time::Instant::now();
        sh.wait_wake(Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
