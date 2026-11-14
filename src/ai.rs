//! Platform-independent AI client: config file, OpenAI-format streaming and
//! provider failover. No Windows code in here, so it is unit-tested on any OS.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    time::Duration,
};

const IMAGE_URL_PREFIX: &str = "data:image/jpeg;base64,";

/// One provider entry. `name` is also the key name in Credential Manager.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCfg {
    pub name: String,
    /// Base URL of an OpenAI-compatible API, without `/chat/completions`.
    pub base_url: String,
    pub model: String,
    /// Whether this model accepts images. Only these are used for screen images.
    #[serde(default)]
    pub vision: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub system_prompt: String,
    /// Tried in this order. When one fails (limit, bad key, outage) the next is used.
    pub providers: Vec<ProviderCfg>,
}

impl Default for Config {
    fn default() -> Self {
        let gemini = |model: &str| ProviderCfg {
            name: "gemini".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            model: model.into(),
            vision: true,
        };
        Config {
            system_prompt: "You are a friendly, practical assistant that lives on the user's screen. \
Reply in the language the user writes in (English, Hindi or Hinglish). Keep answers short unless asked \
for more. If text read from the user's screen is included it comes from OCR, so it may contain mistakes \
and unrelated menu or button text; use what is relevant. When asked to write an email or message, \
give only the ready-to-send text. Never use emojis."
                .into(),
            providers: vec![
                gemini("gemini-3.5-flash-lite"),
                gemini("gemini-3.8-flash"),
            ],
        }
    }
}

/// `%APPDATA%\AI Companion\config.json`
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("AI Companion").join("config.json"))
}

/// Reads the config. On first run the defaults are written so they can be edited
/// in Notepad. A broken file falls back to the defaults.
pub fn load_config() -> Config {
    let default = Config::default();
    let Some(path) = config_path() else { return default };
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Config>(&text) {
            Ok(cfg) if !cfg.providers.is_empty() => cfg,
            Ok(_) => default,
            Err(error) => {
                eprintln!("config.json is invalid ({error}); using defaults");
                default
            }
        },
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(text) = serde_json::to_string_pretty(&default) {
                let _ = std::fs::write(&path, text);
            }
            default
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub role: String,
    pub content: String,
}

impl Msg {
    pub fn user(content: impl Into<String>) -> Self {
        Msg { role: "user".into(), content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Msg { role: "assistant".into(), content: content.into() }
    }
}

/// Builds the `messages` array. The image, if any, is attached to the last user message.
pub fn build_messages(system: &str, history: &[Msg], image_b64: Option<&str>) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    let last_user = history.iter().rposition(|m| m.role == "user");
    for (index, msg) in history.iter().enumerate() {
        if let (Some(b64), true) = (image_b64, Some(index) == last_user) {
            out.push(json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": msg.content },
                    { "type": "image_url", "image_url": { "url": format!("{IMAGE_URL_PREFIX}{b64}") } }
                ]
            }));
        } else {
            out.push(json!({ "role": msg.role, "content": msg.content }));
        }
    }
    out
}

#[derive(Debug)]
enum AttemptError {
    /// The provider refused or returned nothing usable; safe to try the next one.
    Rejected { status: u16, detail: String },
    /// Could not reach the provider.
    Network(String),
    /// The stream broke after some text had already been shown.
    Midstream { partial: String, message: String },
}

fn describe(error: &AttemptError) -> String {
    match error {
        AttemptError::Rejected { status, detail } => {
            let what = match status {
                401 | 403 => "key rejected".to_string(),
                429 => "limit reached".to_string(),
                404 => "model not found".to_string(),
                0 | 200 => "no usable reply".to_string(),
                other => format!("HTTP {other}"),
            };
            if detail.is_empty() { what } else { format!("{what} - {detail}") }
        }
        AttemptError::Network(message) => format!("network error - {message}"),
        AttemptError::Midstream { message, .. } => format!("stopped early - {message}"),
    }
}

fn short(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(160).collect()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        // Honour HTTPS_PROXY / ALL_PROXY if set (ureq ignores them otherwise).
        .try_proxy_from_env(true)
        .user_agent("ai-companion/0.1")
        .build()
}

fn attempt(
    agent: &ureq::Agent,
    url: &str,
    key: &str,
    body: &str,
    on_token: &mut dyn FnMut(&str),
) -> Result<String, AttemptError> {
    let response = match agent
        .post(url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_string(body)
    {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let mut text = String::new();
            let _ = response.into_reader().take(2000).read_to_string(&mut text);
            return Err(AttemptError::Rejected { status, detail: short(&text) });
        }
        Err(ureq::Error::Transport(transport)) => {
            return Err(AttemptError::Network(transport.to_string()));
        }
    };

    let mut full = String::new();
    for line in BufReader::new(response.into_reader()).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                return Err(if full.is_empty() {
                    AttemptError::Network(short(&error.to_string()))
                } else {
                    AttemptError::Midstream { partial: full, message: short(&error.to_string()) }
                });
            }
        };
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(error) = value.get("error") {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("provider error");
            return Err(if full.is_empty() {
                AttemptError::Rejected { status: 0, detail: short(message) }
            } else {
                AttemptError::Midstream { partial: full, message: short(message) }
            });
        }
        if let Some(piece) = value["choices"][0]["delta"]["content"].as_str() {
            if !piece.is_empty() {
                full.push_str(piece);
                on_token(piece);
            }
        }
    }

    if full.is_empty() {
        Err(AttemptError::Rejected { status: 0, detail: String::new() })
    } else {
        Ok(full)
    }
}

#[derive(Debug)]
pub struct Reply {
    pub provider: String,
    pub model: String,
    pub text: String,
    /// Set when the stream stopped early but some text was received.
    pub warning: Option<String>,
}

/// Sends the conversation to the first provider that works, streaming tokens to
/// `on_token`. `on_notice` reports a failover. Providers without a key are skipped.
pub fn chat(
    cfg: &Config,
    history: &[Msg],
    image_b64: Option<&str>,
    key_for: &dyn Fn(&str) -> Option<String>,
    on_token: &mut dyn FnMut(&str),
    on_notice: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let agent = agent();
    let mut problems: Vec<String> = Vec::new();
    let mut missing_keys: Vec<String> = Vec::new();
    let mut eligible = 0;

    for provider in &cfg.providers {
        if image_b64.is_some() && !provider.vision {
            continue;
        }
        eligible += 1;
        let Some(key) = key_for(&provider.name) else {
            if !missing_keys.contains(&provider.name) {
                missing_keys.push(provider.name.clone());
            }
            continue;
        };
        let label = format!("{} ({})", provider.name, provider.model);
        let body = json!({
            "model": provider.model,
            "messages": build_messages(&cfg.system_prompt, history, image_b64),
            "stream": true,
        })
        .to_string();
        let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

        match attempt(&agent, &url, &key, &body, on_token) {
            Ok(text) => {
                return Ok(Reply { provider: provider.name.clone(), model: provider.model.clone(), text, warning: None });
            }
            Err(error) => {
                let reason = describe(&error);
                on_notice(&format!("{label}: {reason}. Trying the next one..."));
                problems.push(format!("{label}: {reason}"));
            }
        }
    }

    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    if eligible == 0 {
        return Err("No provider in config.json can read images. Add one with \"vision\": true.".into());
    }
    let kind = if image_b64.is_some() { "Screen images need a Gemini key. " } else { "" };
    Err(format!("{kind}No API key set for: {}. Click the gear icon and paste a key.", missing_keys.join(", ")))
}

/// Standard base64 (with padding), used to embed the screenshot JPEG.
pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
        thread,
    };

    /// Reads one HTTP request (headers and body) and returns it as text.
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

    /// Serves one canned (status, body) per incoming connection. Returns the base
    /// URL and the list of raw requests received.
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

    fn sse(pieces: &[&str], done: bool) -> String {
        let mut s = String::new();
        for p in pieces {
            s.push_str(&format!("data: {}\n\n", json!({"choices":[{"delta":{"content":p}}]})));
        }
        if done {
            s.push_str("data: [DONE]\n\n");
        }
        s
    }

    fn provider(name: &str, base: &str, vision: bool) -> ProviderCfg {
        ProviderCfg { name: name.into(), base_url: base.into(), model: format!("{name}-model"), vision }
    }

    fn cfg(providers: Vec<ProviderCfg>) -> Config {
        Config { system_prompt: "sys".into(), providers }
    }

    fn keys(name: &str) -> Option<String> {
        Some(format!("key-{name}"))
    }

    #[test]
    fn streams_tokens_in_order() {
        let (base, seen) = serve(vec![(200, sse(&["Hel", "lo ", "world"], true))]);
        let mut tokens = Vec::new();
        let reply = chat(
            &cfg(vec![provider("a", &base, false)]),
            &[Msg::user("hi")],
            None,
            &keys,
            &mut |t| tokens.push(t.to_string()),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply.text, "Hello world");
        assert_eq!(tokens, vec!["Hel", "lo ", "world"]);
        assert_eq!(reply.provider, "a");
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.starts_with("POST /v1/chat/completions"), "{request}");
        assert!(request.to_ascii_lowercase().contains("authorization: bearer key-a"));
        assert!(request.contains("\"stream\":true"));
        assert!(request.contains("\"model\":\"a-model\""));
    }

    #[test]
    fn stream_without_done_marker_still_returns_text() {
        let (base, _) = serve(vec![(200, sse(&["ok"], false))]);
        let reply = chat(&cfg(vec![provider("a", &base, false)]), &[Msg::user("hi")], None, &keys, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(reply.text, "ok");
    }

    #[test]
    fn fails_over_after_rate_limit() {
        let (base_a, _) = serve(vec![(429, "{\"error\":{\"message\":\"slow down\"}}".into())]);
        let (base_b, seen_b) = serve(vec![(200, sse(&["from b"], true))]);
        let mut notices = Vec::new();
        let reply = chat(
            &cfg(vec![provider("a", &base_a, false), provider("b", &base_b, false)]),
            &[Msg::user("hi")],
            None,
            &keys,
            &mut |_| {},
            &mut |n| notices.push(n.to_string()),
        )
        .unwrap();
        assert_eq!(reply.provider, "b");
        assert_eq!(reply.text, "from b");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("limit reached"), "{notices:?}");
        assert!(seen_b.lock().unwrap()[0].to_ascii_lowercase().contains("bearer key-b"));
    }

    #[test]
    fn reports_every_failure_when_all_fail() {
        let (base_a, _) = serve(vec![(401, "bad key".into())]);
        let (base_b, _) = serve(vec![(500, "boom".into())]);
        let err = chat(
            &cfg(vec![provider("a", &base_a, false), provider("b", &base_b, false)]),
            &[Msg::user("hi")],
            None,
            &keys,
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("a (a-model): key rejected"), "{err}");
        assert!(err.contains("b (b-model): HTTP 500"), "{err}");
    }

    #[test]
    fn unreachable_provider_is_a_network_failure_and_fails_over() {
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let (base_b, _) = serve(vec![(200, sse(&["alive"], true))]);
        let reply = chat(
            &cfg(vec![provider("a", &format!("http://127.0.0.1:{dead_port}/v1"), false), provider("b", &base_b, false)]),
            &[Msg::user("hi")],
            None,
            &keys,
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply.text, "alive");
    }

    #[test]
    fn missing_keys_give_a_clear_message() {
        let err = chat(
            &cfg(vec![provider("groq", "http://127.0.0.1:1/v1", false), provider("gemini", "http://127.0.0.1:1/v1", true)]),
            &[Msg::user("hi")],
            None,
            &|_| None,
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("No API key set for: groq, gemini"), "{err}");
    }

    #[test]
    fn image_requests_skip_text_only_providers_and_attach_the_image() {
        // Text-only provider "a" has no server: if it were called the test would fail.
        let (base_b, seen_b) = serve(vec![(200, sse(&["seen"], true))]);
        let reply = chat(
            &cfg(vec![provider("a", "http://127.0.0.1:1/v1", false), provider("b", &base_b, true)]),
            &[Msg::user("what colour is the button?")],
            Some("QUJD"),
            &keys,
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply.provider, "b");
        let request = seen_b.lock().unwrap()[0].clone();
        assert!(request.contains("\"type\":\"image_url\""), "{request}");
        assert!(request.contains("data:image/jpeg;base64,QUJD"), "{request}");
    }

    #[test]
    fn image_request_without_vision_provider_explains_itself() {
        let err = chat(&cfg(vec![provider("a", "http://127.0.0.1:1/v1", false)]), &[Msg::user("x")], Some("QUJD"), &keys, &mut |_| {}, &mut |_| {}).unwrap_err();
        assert!(err.contains("\"vision\": true"), "{err}");
    }

    #[test]
    fn image_goes_only_on_the_last_user_message() {
        let history = vec![Msg::user("first"), Msg::assistant("answer"), Msg::user("second")];
        let messages = build_messages("sys", &history, Some("QUJD"));
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "first");
        assert!(messages[3]["content"].is_array());
        assert_eq!(messages[3]["content"][0]["text"], "second");
    }

    #[test]
    fn default_config_round_trips_and_has_a_vision_provider() {
        let cfg = Config::default();
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.providers.len(), cfg.providers.len());
        assert!(back.providers.iter().any(|p| p.vision));
        // "vision" may be left out of a hand-written config.
        let minimal: Config = serde_json::from_str(r#"{"system_prompt":"s","providers":[{"name":"x","base_url":"u","model":"m"}]}"#).unwrap();
        assert!(!minimal.providers[0].vision);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }
}