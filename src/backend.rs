//! Runs the chat: takes messages from the chat page, captures the screen when
//! asked, calls the AI client on a worker thread and reports back as `Out`
//! events. No Windows or window-toolkit types in here, so it is unit-tested on
//! any OS. The window layer hands in what it can do (keys, screen capture).

use crate::{
    ai::{self, Config, Msg},
    chat::{self, ChatIpc, ScreenMode},
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// Time for the app's own windows to disappear before the screen is captured.
const HIDE_DELAY: Duration = Duration::from_millis(350);

/// What the backend asks the app to do.
pub enum Out {
    /// A JavaScript call for the chat page, e.g. `window.__ai.token("...")`.
    Js(String),
    /// Hide (false) or show (true) the robot and chat windows around a screen capture.
    Overlays(bool),
    /// The page asked to close the chat window.
    CloseChat,
}

pub type Emit = Arc<dyn Fn(Out) + Send + Sync>;

/// What the platform layer provides. Plain functions so tests can swap them.
#[derive(Clone, Copy)]
pub struct Deps {
    pub key_get: fn(&str) -> Option<String>,
    pub key_set: fn(&str, &str) -> Result<(), String>,
    /// Captures the screen and returns the text read from it (OCR).
    pub screen_text: fn() -> Result<String, String>,
    /// Captures the screen and returns a base64 JPEG.
    pub screen_jpeg: fn() -> Result<String, String>,
}

pub struct Backend {
    cfg: Arc<Config>,
    deps: Deps,
    emit: Emit,
    /// Shown in the chat page, e.g. "Alt+Space".
    hotkey: String,
    history: Arc<Mutex<Vec<Msg>>>,
    busy: Arc<AtomicBool>,
    /// Bumped by `reset`, so a reply that finishes after a reset is dropped.
    generation: Arc<AtomicU64>,
}

/// Clears the busy flag however the worker thread ends.
struct BusyGuard(Arc<AtomicBool>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Backend {
    pub fn new(cfg: Config, deps: Deps, emit: Emit, hotkey: String) -> Self {
        Backend {
            cfg: Arc::new(cfg),
            deps,
            emit,
            hotkey,
            history: Arc::new(Mutex::new(Vec::new())),
            busy: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Forgets the conversation. Also called when the chat window is closed,
    /// because the window (and the text it shows) is destroyed to free memory.
    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.history.lock().unwrap().clear();
    }

    /// Handles one message from the chat page. Never blocks: real work runs on a thread.
    pub fn handle(&self, raw: &str) {
        let Some(message) = chat::parse_ipc(raw) else { return };
        match message {
            ChatIpc::Ready => self.send_state(),
            ChatIpc::Close => (self.emit)(Out::CloseChat),
            ChatIpc::NewChat => self.reset(),
            ChatIpc::SetKey { provider, key } => {
                // Only provider names from config.json are accepted.
                if !self.cfg.providers.iter().any(|p| p.name == provider) {
                    (self.emit)(Out::Js(chat::js_error(&format!("Unknown provider: {provider}"))));
                    return;
                }
                match (self.deps.key_set)(&provider, &key) {
                    Ok(()) => self.send_state(),
                    Err(error) => (self.emit)(Out::Js(chat::js_error(&format!("Could not save the key: {error}")))),
                }
            }
            ChatIpc::Send { text, screen } => self.start_send(text, screen),
        }
    }

    fn send_state(&self) {
        let has = |name: &str| (self.deps.key_get)(name).is_some();
        (self.emit)(Out::Js(chat::js_state(&self.hotkey, has("groq"), has("gemini"))));
    }

    fn start_send(&self, text: String, screen: ScreenMode) {
        if self.busy.swap(true, Ordering::SeqCst) {
            (self.emit)(Out::Js(chat::js_error("Still answering the last message.")));
            return;
        }
        let guard = BusyGuard(self.busy.clone());
        let cfg = self.cfg.clone();
        let deps = self.deps;
        let emit = self.emit.clone();
        let history = self.history.clone();
        let generation = self.generation.clone();
        thread::spawn(move || {
            let _guard = guard;
            run_send(&cfg, deps, &emit, &history, &generation, text, screen);
        });
    }
}

fn run_send(
    cfg: &Config,
    deps: Deps,
    emit: &Emit,
    history: &Mutex<Vec<Msg>>,
    generation: &AtomicU64,
    text: String,
    screen: ScreenMode,
) {
    let started_in = generation.load(Ordering::SeqCst);
    let mut content = text.clone();
    let mut image: Option<String> = None;

    if screen != ScreenMode::Off {
        emit(Out::Js(chat::js_notice("Looking at your screen...")));
        emit(Out::Overlays(false));
        thread::sleep(HIDE_DELAY);
        let captured = match screen {
            ScreenMode::Text => (deps.screen_text)().map(|screen_text| content = chat::with_screen_text(&text, &screen_text)),
            _ => (deps.screen_jpeg)().map(|b64| image = Some(b64)),
        };
        emit(Out::Overlays(true));
        if let Err(error) = captured {
            emit(Out::Js(chat::js_error(&format!("Screen read failed: {error}"))));
            return;
        }
    }

    let snapshot = {
        let mut h = history.lock().unwrap();
        h.push(Msg::user(content));
        chat::trim_history(&mut h);
        h.clone()
    };

    let key_for = |name: &str| (deps.key_get)(name);
    let result = ai::chat(
        cfg,
        &snapshot,
        image.as_deref(),
        &key_for,
        &mut |token| emit(Out::Js(chat::js_token(token))),
        &mut |notice| emit(Out::Js(chat::js_notice(notice))),
    );

    let mut h = history.lock().unwrap();
    let still_current = generation.load(Ordering::SeqCst) == started_in;
    match result {
        Ok(reply) => {
            if still_current {
                h.push(Msg::assistant(reply.text.clone()));
            }
            emit(Out::Js(chat::js_done(&reply.provider, &reply.model, reply.warning.as_deref())));
        }
        Err(message) => {
            if still_current {
                h.pop(); // the question was not answered; do not keep it as context
            }
            emit(Out::Js(chat::js_error(&message)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
    };

    fn read_request(stream: &mut TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).to_string();
            if let Some(split) = text.find("\r\n\r\n") {
                let length = text[..split]
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if data.len() >= split + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).to_string()
    }

    /// One canned (status, body) per connection; returns the base URL and the requests seen.
    fn serve(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = seen.clone();
        thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                seen_thread.lock().unwrap().push(read_request(&mut stream));
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), seen)
    }

    fn sse(pieces: &[&str]) -> String {
        let mut s = String::new();
        for p in pieces {
            s.push_str(&format!("data: {}\n\n", json!({"choices":[{"delta":{"content":p}}]})));
        }
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn cfg(base: &str, vision: bool) -> Config {
        Config {
            system_prompt: "sys".into(),
            providers: vec![
                ai::ProviderCfg { name: "groq".into(), base_url: base.into(), model: "m".into(), vision },
                ai::ProviderCfg { name: "gemini".into(), base_url: "http://127.0.0.1:1/v1".into(), model: "g".into(), vision: true },
            ],
        }
    }

    fn deps() -> Deps {
        Deps {
            key_get: |_| Some("secret".into()),
            key_set: |_, _| Ok(()),
            screen_text: || Ok("INVOICE 42".into()),
            screen_jpeg: || Ok("QUJD".into()),
        }
    }

    #[derive(Debug)]
    enum Ev {
        Js(String),
        Overlays(bool),
        Close,
    }

    fn backend(cfg: Config, deps: Deps) -> (Backend, mpsc::Receiver<Ev>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let emit: Emit = Arc::new(move |out| {
            let ev = match out {
                Out::Js(js) => Ev::Js(js),
                Out::Overlays(v) => Ev::Overlays(v),
                Out::CloseChat => Ev::Close,
            };
            let _ = tx.lock().unwrap().send(ev);
        });
        (Backend::new(cfg, deps, emit, "Alt+Space".into()), rx)
    }

    /// Collects events until the reply is finished (`done` or `error`).
    fn until_finished(rx: &mpsc::Receiver<Ev>) -> Vec<Ev> {
        let mut out = Vec::new();
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(10)).expect("timed out waiting for the reply");
            let finished = matches!(&ev, Ev::Js(js) if js.starts_with("window.__ai.done(") || js.starts_with("window.__ai.error("));
            out.push(ev);
            if finished {
                return out;
            }
        }
    }

    fn js_of(events: &[Ev]) -> Vec<&str> {
        events.iter().filter_map(|e| if let Ev::Js(js) = e { Some(js.as_str()) } else { None }).collect()
    }

    fn wait_idle(b: &Backend) {
        while b.busy.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn send(text: &str, screen: &str) -> String {
        json!({"t":"send","text":text,"screen":screen}).to_string()
    }

    #[test]
    fn plain_message_streams_and_is_remembered() {
        let (base, seen) = serve(vec![(200, sse(&["Hi ", "there"])), (200, sse(&["again"]))]);
        let (b, rx) = backend(cfg(&base, false), deps());
        b.handle(&send("hello", ""));
        let events = until_finished(&rx);
        let js = js_of(&events);
        assert_eq!(js[0], r#"window.__ai.token("Hi ")"#);
        assert_eq!(js[1], r#"window.__ai.token("there")"#);
        assert!(js.last().unwrap().starts_with(r#"window.__ai.done({provider:"groq",model:"m""#), "{js:?}");
        assert!(!events.iter().any(|e| matches!(e, Ev::Overlays(_))), "no screen capture unless asked");

        wait_idle(&b);
        b.handle(&send("and now?", ""));
        until_finished(&rx);
        let second = seen.lock().unwrap()[1].clone();
        assert!(second.contains("hello") && second.contains("Hi there") && second.contains("and now?"), "{second}");
    }

    #[test]
    fn screen_text_is_added_to_the_question_and_windows_are_hidden_then_shown() {
        let (base, seen) = serve(vec![(200, sse(&["ok"]))]);
        let (b, rx) = backend(cfg(&base, false), deps());
        b.handle(&send("reply to this email", "text"));
        let events = until_finished(&rx);
        let overlays: Vec<bool> = events.iter().filter_map(|e| if let Ev::Overlays(v) = e { Some(*v) } else { None }).collect();
        assert_eq!(overlays, vec![false, true]);
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.contains("reply to this email"), "{request}");
        assert!(request.contains("INVOICE 42"), "{request}");
        assert!(!request.contains("image_url"), "text mode must not send an image");
    }

    #[test]
    fn screen_image_is_attached_for_vision_providers() {
        let (base, seen) = serve(vec![(200, sse(&["blue"]))]);
        let (b, rx) = backend(cfg(&base, true), deps());
        b.handle(&send("what colour is the button?", "image"));
        until_finished(&rx);
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.contains("data:image/jpeg;base64,QUJD"), "{request}");
        assert!(!request.contains("INVOICE"), "image mode must not send OCR text");
    }

    #[test]
    fn failed_screen_capture_shows_an_error_restores_windows_and_sends_nothing() {
        let mut d = deps();
        d.screen_text = || Err("no display".into());
        let (b, rx) = backend(cfg("http://127.0.0.1:1/v1", false), d);
        b.handle(&send("x", "text"));
        let events = until_finished(&rx);
        let last = js_of(&events).last().copied().unwrap();
        assert!(last.starts_with("window.__ai.error(") && last.contains("no display"), "{last}");
        assert!(events.iter().any(|e| matches!(e, Ev::Overlays(true))), "windows must be shown again after a failure");
        assert!(b.history.lock().unwrap().is_empty());
    }

    #[test]
    fn provider_error_is_reported_and_the_question_is_not_kept() {
        let (base, _) = serve(vec![(401, "bad key".into())]);
        let mut c = cfg(&base, false);
        c.providers.truncate(1);
        let (b, rx) = backend(c, deps());
        b.handle(&send("hello", ""));
        let events = until_finished(&rx);
        let last = js_of(&events).last().copied().unwrap();
        assert!(last.starts_with("window.__ai.error(") && last.contains("key rejected"), "{last}");
        wait_idle(&b);
        assert!(b.history.lock().unwrap().is_empty());
    }

    #[test]
    fn a_second_message_while_busy_is_refused() {
        let (b, rx) = backend(cfg("http://127.0.0.1:1/v1", false), deps());
        b.busy.store(true, Ordering::SeqCst);
        b.handle(&send("hello", ""));
        let Ev::Js(js) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!("expected a js event") };
        assert!(js.starts_with("window.__ai.error(") && js.contains("Still answering"), "{js}");
    }

    #[test]
    fn ready_reports_which_keys_exist_without_revealing_them() {
        let mut d = deps();
        d.key_get = |name| if name == "groq" { Some("secret-value".into()) } else { None };
        let (b, rx) = backend(cfg("http://x/v1", false), d);
        b.handle(r#"{"t":"ready"}"#);
        let Ev::Js(js) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!() };
        assert_eq!(js, r#"window.__ai.state({hotkey:"Alt+Space",groq:true,gemini:false})"#);
        assert!(!js.contains("secret-value"));
    }

    #[test]
    fn saving_a_key_only_works_for_providers_in_the_config() {
        use std::sync::atomic::AtomicUsize;
        static SAVED: AtomicUsize = AtomicUsize::new(0);
        let mut d = deps();
        d.key_set = |_, _| {
            SAVED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let (b, rx) = backend(cfg("http://x/v1", false), d);
        b.handle(r#"{"t":"key","provider":"evil","key":"k"}"#);
        let Ev::Js(js) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!() };
        assert!(js.starts_with("window.__ai.error("), "{js}");
        assert_eq!(SAVED.load(Ordering::SeqCst), 0);
        b.handle(r#"{"t":"key","provider":"groq","key":"k"}"#);
        let Ev::Js(js) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!() };
        assert!(js.starts_with("window.__ai.state("), "{js}");
        assert_eq!(SAVED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_key_save_failure_is_shown_to_the_user() {
        let mut d = deps();
        d.key_set = |_, _| Err("vault locked".into());
        let (b, rx) = backend(cfg("http://x/v1", false), d);
        b.handle(r#"{"t":"key","provider":"groq","key":"k"}"#);
        let Ev::Js(js) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!() };
        assert!(js.contains("vault locked"), "{js}");
    }

    #[test]
    fn new_chat_forgets_the_conversation_and_close_asks_the_app_to_close_the_window() {
        let (b, rx) = backend(cfg("http://x/v1", false), deps());
        b.history.lock().unwrap().push(Msg::user("old"));
        b.handle(r#"{"t":"new"}"#);
        assert!(b.history.lock().unwrap().is_empty());
        assert_eq!(b.generation.load(Ordering::SeqCst), 1);
        b.handle(r#"{"t":"close"}"#);
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Ev::Close));
    }

    #[test]
    fn old_messages_are_dropped_and_context_starts_with_a_user_message() {
        let (base, seen) = serve(vec![(200, sse(&["ok"]))]);
        let (b, rx) = backend(cfg(&base, false), deps());
        {
            let mut h = b.history.lock().unwrap();
            for i in 0..14 {
                h.push(if i % 2 == 0 { Msg::user(format!("u{i}")) } else { Msg::assistant(format!("a{i}")) });
            }
        }
        b.handle(&send("latest", ""));
        until_finished(&rx);
        wait_idle(&b);
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.contains("latest"));
        assert!(!request.contains("\"u0\""), "oldest messages are dropped");
        let h = b.history.lock().unwrap();
        assert!(h.len() <= 9);
        assert_eq!(h.first().unwrap().role, "user");
    }

    #[test]
    fn garbage_from_the_page_is_ignored() {
        let (b, rx) = backend(cfg("http://x/v1", false), deps());
        b.handle("not json");
        b.handle(r#"{"t":"send","text":"   "}"#);
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "nothing should be emitted");
        assert!(!b.busy.load(Ordering::SeqCst));
    }
}
