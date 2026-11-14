//! Glue between the chat page (assets/chat.html) and the app: parsing messages
//! the page sends and building the small JS calls the app sends back.
//! Platform-independent so it is unit-tested on any OS.

use crate::ai::Msg;
use serde_json::Value;

const MAX_TEXT_CHARS: usize = 8_000;
const MAX_SCREEN_CHARS: usize = 12_000;
const MAX_HISTORY: usize = 8;

#[derive(Debug, PartialEq)]
pub enum ScreenMode {
    Off,
    Text,
    Image,
}

#[derive(Debug, PartialEq)]
pub enum WindowAction {
    Minimize,
    Maximize,
    Drag,
}

pub fn parse_window_action(body: &str) -> Option<WindowAction> {
    let value: Value = serde_json::from_str(body).ok()?;
    if value["t"].as_str()? != "window" {
        return None;
    }
    match value["action"].as_str()? {
        "minimize" => Some(WindowAction::Minimize),
        "maximize" => Some(WindowAction::Maximize),
        "drag" => Some(WindowAction::Drag),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum ChatIpc {
    Ready,
    Close,
    NewChat,
    Translate { text: String },
    Send { text: String, screen: ScreenMode },
    SetKey { provider: String, key: String },
}

/// Parses a JSON message posted by the chat page. Unknown or malformed input is ignored.
pub fn parse_ipc(body: &str) -> Option<ChatIpc> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value["t"].as_str()? {
        "ready" => Some(ChatIpc::Ready),
        "close" => Some(ChatIpc::Close),
        "new" => Some(ChatIpc::NewChat),
        "translate" => {
            let text: String = value["text"].as_str()?.trim().chars().take(MAX_TEXT_CHARS).collect();
            if text.is_empty() {
                return None;
            }
            Some(ChatIpc::Translate { text })
        }
        "send" => {
            let text: String = value["text"].as_str()?.trim().chars().take(MAX_TEXT_CHARS).collect();
            if text.is_empty() {
                return None;
            }
            let screen = match value["screen"].as_str() {
                Some("text") => ScreenMode::Text,
                Some("image") => ScreenMode::Image,
                _ => ScreenMode::Off,
            };
            Some(ChatIpc::Send { text, screen })
        }
        "key" => {
            let provider = value["provider"].as_str()?.to_string();
            let key = value["key"].as_str()?.trim().to_string();
            let valid = !provider.is_empty() && provider.chars().all(|c| c.is_ascii_alphanumeric());
            if valid && !key.is_empty() {
                Some(ChatIpc::SetKey { provider, key })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The user's question together with text read from the screen.
pub fn with_screen_text(question: &str, screen_text: &str) -> String {
    let capped: String = screen_text.trim().chars().take(MAX_SCREEN_CHARS).collect();
    format!(
        "Text read from my screen (OCR, may contain mistakes and unrelated menu text):\n\"\"\"\n{capped}\n\"\"\"\n\nMy request: {question}"
    )
}

/// Keeps the last few messages, always starting with a user message.
pub fn trim_history(history: &mut Vec<Msg>) {
    if history.len() > MAX_HISTORY {
        let extra = history.len() - MAX_HISTORY;
        history.drain(..extra);
    }
    while history.first().is_some_and(|m| m.role != "user") {
        history.remove(0);
    }
}

fn js_str(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

pub fn js_token(text: &str) -> String {
    format!("window.__ai.token({})", js_str(text))
}

pub fn js_notice(text: &str) -> String {
    format!("window.__ai.notice({})", js_str(text))
}

pub fn js_error(text: &str) -> String {
    format!("window.__ai.error({})", js_str(text))
}

pub fn js_translation_result(result: &str, source: &str, grammar_note: &str) -> String {
    format!(
        "window.__ai.translationResult({},{},{})",
        js_str(result),
        js_str(source),
        js_str(grammar_note)
    )
}
pub fn js_done(provider: &str, model: &str, warning: Option<&str>) -> String {
    format!(
        "window.__ai.done({{provider:{},model:{},warning:{}}})",
        js_str(provider),
        js_str(model),
        warning.map(js_str).unwrap_or_else(|| "null".into())
    )
}

pub fn js_state(hotkey: &str, groq: bool, gemini: bool) -> String {
    format!("window.__ai.state({{hotkey:{},groq:{groq},gemini:{gemini}}})", js_str(hotkey))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_send_with_screen_modes() {
        assert_eq!(
            parse_ipc(r#"{"t":"send","text":"  hello  ","screen":"text"}"#),
            Some(ChatIpc::Send { text: "hello".into(), screen: ScreenMode::Text })
        );
        assert_eq!(
            parse_ipc(r#"{"t":"send","text":"x","screen":"image"}"#),
            Some(ChatIpc::Send { text: "x".into(), screen: ScreenMode::Image })
        );
        assert_eq!(
            parse_ipc(r#"{"t":"send","text":"x","screen":"none"}"#),
            Some(ChatIpc::Send { text: "x".into(), screen: ScreenMode::Off })
        );
        assert_eq!(
            parse_ipc(r#"{"t":"send","text":"x"}"#),
            Some(ChatIpc::Send { text: "x".into(), screen: ScreenMode::Off })
        );
    }

    #[test]
    fn rejects_empty_and_malformed_input() {
        assert_eq!(parse_ipc(r#"{"t":"send","text":"   "}"#), None);
        assert_eq!(parse_ipc("not json"), None);
        assert_eq!(parse_ipc(r#"{"t":"unknown"}"#), None);
        assert_eq!(parse_ipc(r#"{"x":1}"#), None);
    }

    #[test]
    fn long_text_is_capped() {
        let body = serde_json::json!({"t":"send","text":"a".repeat(20_000)}).to_string();
        match parse_ipc(&body) {
            Some(ChatIpc::Send { text, .. }) => assert_eq!(text.chars().count(), 8_000),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn key_messages_are_validated() {
        assert_eq!(
            parse_ipc(r#"{"t":"key","provider":"groq","key":" gsk_abc "}"#),
            Some(ChatIpc::SetKey { provider: "groq".into(), key: "gsk_abc".into() })
        );
        assert_eq!(parse_ipc(r#"{"t":"key","provider":"gr oq","key":"k"}"#), None);
        assert_eq!(parse_ipc(r#"{"t":"key","provider":"../x","key":"k"}"#), None);
        assert_eq!(parse_ipc(r#"{"t":"key","provider":"groq","key":"  "}"#), None);
    }

    #[test]
    fn simple_messages() {
        assert_eq!(parse_ipc(r#"{"t":"ready"}"#), Some(ChatIpc::Ready));
        assert_eq!(parse_ipc(r#"{"t":"close"}"#), Some(ChatIpc::Close));
        assert_eq!(parse_ipc(r#"{"t":"new"}"#), Some(ChatIpc::NewChat));
    }

    #[test]
    fn screen_text_is_wrapped_and_capped() {
        let wrapped = with_screen_text("reply politely", "Dear Sir, please send the report");
        assert!(wrapped.contains("Dear Sir, please send the report"));
        assert!(wrapped.ends_with("My request: reply politely"));
        let huge = with_screen_text("q", &"ab".repeat(20_000));
        assert!(huge.chars().count() < 12_300);
    }

    #[test]
    fn history_keeps_recent_messages_and_starts_with_user() {
        let mut history = Vec::new();
        for i in 0..6 {
            history.push(Msg::user(format!("q{i}")));
            history.push(Msg::assistant(format!("a{i}")));
        }
        trim_history(&mut history);
        assert!(history.len() <= 8);
        assert_eq!(history[0].role, "user");
        assert_eq!(history.last().unwrap().content, "a5");
    }

    #[test]
    fn js_helpers_escape_quotes_newlines_and_scripts() {
        let js = js_token("say \"hi\"\n</script><b>");
        assert_eq!(js, r#"window.__ai.token("say \"hi\"\n</script><b>")"#);
        assert_eq!(js_done("groq", "m", None), r#"window.__ai.done({provider:"groq",model:"m",warning:null})"#);
        assert_eq!(
            js_done("g", "m", Some("it broke")),
            r#"window.__ai.done({provider:"g",model:"m",warning:"it broke"})"#
        );
        assert_eq!(js_state("Alt+Space", true, false), r#"window.__ai.state({hotkey:"Alt+Space",groq:true,gemini:false})"#);
    }
}
