//! The USB side: finds the tablet with adb, sets up one `adb forward` and keeps ONE persistent
//! TCP connection open to the DeskBridge keyboard app on the tablet. No adb process per key.
//! Plain threads and blocking std I/O, same as the rest of Aalu (no async runtime).

use super::{lock, proto::Out, Shared};
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::Ordering, mpsc, mpsc::Receiver, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const PORT: u16 = 38917;
/// A burst of mouse moves is merged into one packet at most this often.
const MOUSE_BATCH: Duration = Duration::from_millis(4);

type AdbPath = Arc<Mutex<Option<PathBuf>>>;

pub(crate) fn spawn(sh: Arc<Shared>, rx: Receiver<Out>) {
    let adb: AdbPath = Arc::new(Mutex::new(None));
    {
        let (ime_tx, ime_rx) = mpsc::channel();
        sh.set_ime_queue(ime_tx);
        let (sh, adb) = (sh.clone(), adb.clone());
        let _ = thread::Builder::new().name("deskbridge-ime".into()).spawn(move || ime_worker(sh, adb, ime_rx));
    }
    {
        let sh = sh.clone();
        let _ = thread::Builder::new().name("deskbridge-writer".into()).spawn(move || writer_loop(sh, rx));
    }
    {
        let (sh, adb) = (sh.clone(), adb.clone());
        let _ = thread::Builder::new().name("deskbridge-track".into()).spawn(move || track_loop(sh, adb));
    }
    let _ = thread::Builder::new().name("deskbridge-link".into()).spawn(move || Linker { sh, adb, serial: None }.run());
}

// ------------------------------------------------------------------ finding adb

pub(crate) fn find_adb() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        c.push(dir.join("platform-tools").join("adb.exe"));
        c.push(dir.join("adb.exe"));
    }
    for var in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            c.push(PathBuf::from(v).join("platform-tools").join("adb.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        c.push(PathBuf::from(local).join("Android").join("Sdk").join("platform-tools").join("adb.exe"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            c.push(dir.join("adb.exe"));
        }
    }
    c.into_iter().find(|p| p.is_file())
}

// ------------------------------------------------------------------ running adb

struct Run {
    ok: bool,
    out: String,
    err: String,
}

fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Runs adb with a fixed argument list (no shell, nothing from the tablet is executed).
/// If adb has to start its background server it keeps the output pipes open, so the output is
/// collected on helper threads and only waited for a moment after the process has exited.
fn run_adb(path: &Path, args: &[&str], timeout: Duration) -> Result<Run, String> {
    let mut cmd = Command::new(path);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;

    fn collect(mut r: impl Read + Send + 'static) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            let _ = tx.send(s);
        });
        rx
    }
    let out_rx = collect(child.stdout.take().ok_or("no stdout")?);
    let err_rx = collect(child.stderr.take().ok_or("no stderr")?);

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("adb timed out".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let grace = Duration::from_millis(1500);
    Ok(Run {
        ok: status.success(),
        out: out_rx.recv_timeout(grace).unwrap_or_default(),
        err: err_rx.recv_timeout(grace).unwrap_or_default(),
    })
}

/// `adb start-server` leaves a daemon behind, so its output is not captured at all.
fn start_server(path: &Path) {
    let mut cmd = Command::new(path);
    cmd.arg("start-server").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    hide_window(&mut cmd);
    if let Ok(mut child) = cmd.spawn() {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[derive(Debug, PartialEq)]
struct Dev {
    serial: String,
    state: String,
    model: Option<String>,
}

fn parse_devices(output: &str) -> Vec<Dev> {
    let mut list = Vec::new();
    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("List of") || line.starts_with('*') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else { continue };
        let model = line
            .split_whitespace()
            .find_map(|t| t.strip_prefix("model:"))
            .map(|m| m.replace('_', " "));
        list.push(Dev { serial: serial.to_string(), state: state.to_string(), model });
    }
    list
}

fn serial_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

fn first_line(texts: &[&str]) -> String {
    texts
        .iter()
        .find_map(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("unknown error")
        .to_string()
}

// ------------------------------------------------------------------ keyboard switching

/// The DeskBridge keyboard on the tablet (package/.service).
const DESK_IME: &str = "com.deskbridge.keyboard/.DeskBridgeIME";

pub(crate) enum ImeCmd {
    /// Tablet mode started: remember the current keyboard and select DeskBridge's.
    Enter,
    /// Tablet mode ended: put the remembered keyboard back.
    Leave,
}

/// Input method ids come from the tablet and are passed to its shell, so only plain id characters are accepted.
fn ime_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.contains('/')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '$' | '-'))
}

/// Keyboard to go back to when the tablet had no usable one remembered (e.g. DeskBridge was left
/// selected): the first installed keyboard that is not DeskBridge and not a voice-typing service.
fn pick_fallback(ime_list: &str) -> Option<String> {
    ime_list
        .lines()
        .map(str::trim)
        .find(|id| ime_id_ok(id) && !id.starts_with("com.deskbridge.") && !id.to_lowercase().contains("voice"))
        .map(String::from)
}

fn adb_shell(path: &Path, serial: &str, args: &[&str]) -> Option<String> {
    let mut a = vec!["-s", serial, "shell"];
    a.extend_from_slice(args);
    match run_adb(path, &a, Duration::from_secs(6)) {
        Ok(r) if r.ok => Some(r.out.trim().to_string()),
        Ok(r) => {
            super::log(&format!("adb shell {} failed: {}", args.join(" "), first_line(&[&r.err, &r.out])));
            None
        }
        Err(e) => {
            super::log(&format!("adb shell {} failed: {e}", args.join(" ")));
            None
        }
    }
}

/// One thread runs the jobs in order, so a quick on/off can never swap the keyboard back and forth
/// out of order. adb is slow (a few hundred ms) and must never run on the keyboard hook thread.
fn ime_worker(sh: Arc<Shared>, adb: AdbPath, rx: Receiver<ImeCmd>) {
    let mut previous: Option<String> = None;
    for cmd in rx {
        let (Some(path), Some(serial)) = (lock(&adb).clone(), lock(&sh.serial).clone()) else {
            super::log("keyboard switch skipped: no adb or no tablet");
            continue;
        };
        match cmd {
            ImeCmd::Enter => {
                let current = adb_shell(&path, &serial, &["settings", "get", "secure", "default_input_method"])
                    .filter(|c| c != "null" && ime_id_ok(c) && c != DESK_IME);
                previous = current.or_else(|| {
                    adb_shell(&path, &serial, &["ime", "list", "-s"]).and_then(|l| pick_fallback(&l))
                });
                super::log(&format!("keyboard: switching to DeskBridge, will restore {previous:?}"));
                adb_shell(&path, &serial, &["ime", "set", DESK_IME]);
            }
            ImeCmd::Leave => {
                if let Some(p) = previous.take() {
                    super::log(&format!("keyboard: restoring {p}"));
                    adb_shell(&path, &serial, &["ime", "set", &p]);
                }
            }
        }
    }
}

// ------------------------------------------------------------------ link state machine

struct Linker {
    sh: Arc<Shared>,
    adb: AdbPath,
    serial: Option<String>,
}

impl Linker {
    fn run(mut self) {
        let path = find_adb();
        super::log(&format!("adb path: {}", path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into())));
        *lock(&self.adb) = path.clone();
        if let Some(p) = &path {
            start_server(p);
        }
        loop {
            let wait = self.refresh();
            self.sh.wait_wake(wait);
            thread::sleep(Duration::from_millis(250)); // debounce a burst of device events
        }
    }

    fn fail(&mut self, msg: &str) {
        self.sh.teardown();
        self.serial = None;
        *lock(&self.sh.serial) = None;
        self.sh.link(false, msg);
    }

    fn refresh(&mut self) -> Duration {
        let secs = Duration::from_secs;
        let path = {
            let mut g = lock(&self.adb);
            if g.is_none() {
                *g = find_adb();
            }
            g.clone()
        };
        let Some(path) = path else {
            self.fail("adb.exe not found. Put a 'platform-tools' folder next to aalu.exe or install Android Studio.");
            return secs(5);
        };

        let r = match run_adb(&path, &["devices", "-l"], secs(6)) {
            Ok(r) if r.ok => r,
            Ok(r) => {
                self.fail(&format!("adb failed: {}", first_line(&[&r.err, &r.out])));
                return secs(5);
            }
            Err(e) => {
                self.fail(&format!("adb failed: {e}"));
                return secs(5);
            }
        };

        let devs: Vec<Dev> = parse_devices(&r.out)
            .into_iter()
            .filter(|d| !d.serial.contains(':') && !d.serial.starts_with("emulator-"))
            .collect();
        if devs.is_empty() {
            self.fail("No tablet found. Plug in the USB cable and unlock the tablet.");
            return secs(20);
        }
        if devs.len() > 1 {
            self.fail("More than one Android device is connected over USB. Unplug the others.");
            return secs(10);
        }
        let d = &devs[0];
        if !serial_ok(&d.serial) {
            self.fail("Unexpected device id from adb.");
            return secs(10);
        }
        *lock(&self.sh.serial) = Some(d.serial.clone());
        if d.state == "unauthorized" {
            self.fail("Unlock the tablet and tap 'Allow' on the USB debugging prompt (tick 'Always allow from this computer').");
            return secs(20);
        }
        if d.state != "device" {
            self.fail(&format!("Device state is '{}'. Re-plug the cable or toggle USB debugging off and on.", d.state));
            return secs(5);
        }

        // Already connected to this device and the socket is still alive?
        if self.sh.connected.load(Ordering::SeqCst) && self.serial.as_deref() == Some(d.serial.as_str()) {
            self.sh.link(true, "Connected. Ready.");
            return secs(20);
        }

        self.sh.teardown();
        self.serial = Some(d.serial.clone());

        let fwd = format!("tcp:{PORT}");
        match run_adb(&path, &["-s", &d.serial, "forward", &fwd, "localabstract:deskbridge"], secs(5)) {
            Ok(f) if f.ok => {}
            Ok(f) => {
                self.sh.link(false, &format!("adb forward failed: {}", first_line(&[&f.err, &f.out])));
                return secs(5);
            }
            Err(e) => {
                self.sh.link(false, &format!("adb forward failed: {e}"));
                return secs(5);
            }
        }

        if handshake(&self.sh, SocketAddr::from(([127, 0, 0, 1], PORT))) {
            self.sh.link(true, "Connected. Ready.");
            return secs(20);
        }
        self.sh.link(
            false,
            "Tablet found, but the DeskBridge keyboard is not active yet. On the tablet: enable 'DeskBridge Keyboard', then tap a text field and choose it in the keyboard switcher.",
        );
        Duration::from_millis(2500) // retry soon while waiting for the keyboard
    }
}

/// Connects to the tablet keyboard and waits for its 0x01 "hello" byte.
fn handshake(sh: &Arc<Shared>, addr: SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return false };
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut hello = [0u8; 1];
    if s.read_exact(&mut hello).is_err() || hello[0] != 1 {
        return false;
    }
    let _ = s.set_read_timeout(None);
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(reader) = s.try_clone() else { return false };
    let gen = sh.install_stream(s);
    start_reader(sh.clone(), reader, gen);
    true
}

/// The tablet never sends anything after "hello"; this only notices the socket closing.
fn start_reader(sh: Arc<Shared>, mut r: TcpStream, gen: u64) {
    let _ = thread::Builder::new().name("deskbridge-reader".into()).spawn(move || {
        let mut b = [0u8; 16];
        while matches!(r.read(&mut b), Ok(n) if n > 0) {}
        if sh.conn_gen.load(Ordering::SeqCst) == gen {
            sh.teardown();
            sh.wake();
        }
    });
}

/// Event driven: `adb track-devices` prints something whenever a device appears or goes.
fn track_loop(sh: Arc<Shared>, adb: AdbPath) {
    loop {
        let path = lock(&adb).clone();
        let Some(path) = path else {
            thread::sleep(Duration::from_secs(3));
            continue;
        };
        let mut cmd = Command::new(&path);
        cmd.arg("track-devices").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        hide_window(&mut cmd);
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut out) = child.stdout.take() {
                let mut buf = [0u8; 256];
                while matches!(out.read(&mut buf), Ok(n) if n > 0) {
                    sh.wake();
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        sh.wake();
        thread::sleep(Duration::from_secs(3));
    }
}

// ------------------------------------------------------------------ writing

fn writer_loop(sh: Arc<Shared>, rx: Receiver<Out>) {
    let mut cache: Option<(u64, TcpStream)> = None;
    let mut pending: Option<Out> = None;
    loop {
        let mut cur = match pending.take() {
            Some(p) => p,
            None => match rx.recv() {
                Ok(p) => p,
                Err(_) => return,
            },
        };
        if matches!(cur, Out::Mouse { .. }) {
            thread::sleep(MOUSE_BATCH);
            while let Ok(next) = rx.try_recv() {
                if !cur.merge(&next) {
                    pending = Some(next);
                    break;
                }
            }
        }
        write_packet(&sh, &mut cache, &cur.encode());
    }
}

fn write_packet(sh: &Arc<Shared>, cache: &mut Option<(u64, TcpStream)>, bytes: &[u8]) {
    let gen = sh.conn_gen.load(Ordering::SeqCst);
    if cache.as_ref().map(|c| c.0) != Some(gen) {
        *cache = lock(&sh.stream).as_ref().and_then(|s| s.try_clone().ok()).map(|s| (gen, s));
    }
    let Some((g, s)) = cache.as_mut() else { return };
    if s.write_all(bytes).is_err() {
        let g = *g;
        *cache = None;
        if sh.conn_gen.load(Ordering::SeqCst) == g {
            sh.teardown();
            sh.wake();
        }
    }
}

impl Shared {
    /// Closes the connection (if any). Keyboard goes back to the laptop.
    pub(crate) fn teardown(&self) {
        let old = lock(&self.stream).take();
        self.conn_gen.fetch_add(1, Ordering::SeqCst);
        if let Some(s) = old {
            let _ = s.shutdown(Shutdown::Both);
        }
        if self.connected.swap(false, Ordering::SeqCst) {
            self.set_tablet(false); // never leave the laptop keyboard dead
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Event;
    use std::net::TcpListener;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn parses_adb_devices_output() {
        let out = "* daemon not running; starting now at tcp:5037\n* daemon started successfully\nList of devices attached\nABC123\tdevice usb:1-2 product:pipa model:Xiaomi_Pad_7 device:pipa transport_id:1\nXYZ\tunauthorized usb:1-3 transport_id:2\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], Dev { serial: "ABC123".into(), state: "device".into(), model: Some("Xiaomi Pad 7".into()) });
        assert_eq!(d[1].state, "unauthorized");
        assert_eq!(d[1].model, None);
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn keyboard_fallback_skips_deskbridge_and_voice_input() {
        let list = "com.deskbridge.keyboard/.DeskBridgeIME\ncom.google.android.tts/com.google.android.apps.speech.tts.googletts.settings.asr.voiceime.VoiceInputMethodService\ncom.google.android.inputmethod.latin/com.android.inputmethod.latin.LatinIME\n";
        assert_eq!(
            pick_fallback(list).as_deref(),
            Some("com.google.android.inputmethod.latin/com.android.inputmethod.latin.LatinIME")
        );
        assert_eq!(pick_fallback("com.deskbridge.keyboard/.DeskBridgeIME\n"), None);
        assert_eq!(pick_fallback(""), None);
    }

    #[test]
    fn keyboard_ids_are_checked_before_reaching_the_tablet_shell() {
        assert!(ime_id_ok(DESK_IME));
        assert!(ime_id_ok("com.a.b/com.a.b.Inner$Service"));
        assert!(!ime_id_ok("null"));
        assert!(!ime_id_ok("a/b; reboot"));
        assert!(!ime_id_ok("a/b c"));
        assert!(!ime_id_ok(""));
    }

    #[test]
    fn serial_and_first_line() {
        assert!(serial_ok("ABC123.x-1:5555"));
        assert!(!serial_ok("a b"));
        assert!(!serial_ok("a;rm"));
        assert_eq!(first_line(&["", "  \n err one\nerr two", "x"]), "err one");
        assert_eq!(first_line(&["", ""]), "unknown error");
    }

    #[cfg(unix)]
    #[test]
    fn run_adb_collects_output_and_times_out() {
        let r = run_adb(Path::new("echo"), &["hi"], Duration::from_secs(5)).unwrap();
        assert!(r.ok && r.out.trim() == "hi");
        assert!(run_adb(Path::new("sleep"), &["5"], Duration::from_millis(200)).is_err());
    }

    /// Fake tablet: sends hello, then records what it receives.
    fn fake_tablet() -> (SocketAddr, mpsc::Receiver<Vec<u8>>, mpsc::Sender<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (data_tx, data_rx) = mpsc::channel();
        let (close_tx, close_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&[1]).unwrap();
            s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            let mut buf = [0u8; 256];
            loop {
                if close_rx.try_recv().is_ok() {
                    return; // drops the socket
                }
                if let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    let _ = data_tx.send(buf[..n].to_vec());
                }
            }
        });
        (addr, data_rx, close_tx)
    }

    fn shared() -> (Arc<Shared>, Receiver<Out>, mpsc::Receiver<Event>) {
        let (tx, rx) = sync_channel(64);
        let (ev_tx, ev_rx) = mpsc::channel();
        let ev_tx = Mutex::new(ev_tx);
        let sh = Arc::new(Shared::new(tx, Box::new(move |e| {
            let _ = ev_tx.lock().unwrap().send(e);
        })));
        (sh, rx, ev_rx)
    }

    #[test]
    fn packets_flow_to_the_tablet_and_a_closed_socket_disconnects() {
        let (addr, data, close) = fake_tablet();
        let (sh, rx, events) = shared();
        {
            let sh = sh.clone();
            thread::spawn(move || writer_loop(sh, rx));
        }
        assert!(!sh.send(Out::key(1, 0)), "nothing is sent while disconnected");
        assert!(handshake(&sh, addr));
        assert!(sh.connected.load(Ordering::SeqCst));

        assert!(sh.send(Out::key(67, 0)));
        let got = data.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got, Out::key(67, 0).encode());

        // a burst of moves arrives merged
        for _ in 0..5 {
            sh.send(Out::Mouse { dx: 1, dy: 2, buttons: 0, wheel: 0 });
        }
        let mut total = (0i32, 0i32);
        let deadline = Instant::now() + Duration::from_secs(2);
        while total != (5, 10) && Instant::now() < deadline {
            if let Ok(p) = data.recv_timeout(Duration::from_millis(200)) {
                for m in p.chunks(8) {
                    assert_eq!(m[0], b'M');
                    total.0 += i16::from_be_bytes([m[1], m[2]]) as i32;
                    total.1 += i16::from_be_bytes([m[3], m[4]]) as i32;
                }
            }
        }
        assert_eq!(total, (5, 10));

        close.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while sh.connected.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!sh.connected.load(Ordering::SeqCst), "reader must notice the closed socket");
        let _ = events; // link/mode events are covered in mod.rs tests
    }

    #[test]
    fn handshake_fails_without_a_listener() {
        let (sh, _rx, _ev) = shared();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        assert!(!handshake(&sh, addr));
        assert!(!sh.connected.load(Ordering::SeqCst));
    }
}
