#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopWindowTarget},
    window::{Window, WindowBuilder},
};
use wry::{WebView, WebViewBuilder};

const ROBOT_HTML: &str = include_str!("../assets/robot.html");
const CHAT_HTML: &str = include_str!("../assets/chat.html");

fn main() -> wry::Result<()> {
    let event_loop = EventLoop::new();
    let (robot_tx, robot_rx) = mpsc::channel::<String>();

    let robot_window = WindowBuilder::new()
        .with_title("AI Companion")
        .with_inner_size(LogicalSize::new(120.0, 140.0))
        .with_position(LogicalPosition::new(40.0, 120.0))
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .build(&event_loop)
        .expect("create robot window");
    let _robot_webview = WebViewBuilder::new()
        .with_html(ROBOT_HTML)
        .with_transparent(true)
        .with_ipc_handler(move |request| { let _ = robot_tx.send(request.body().clone()); })
        .build(&robot_window)?;

    let mut chat_window: Option<Window> = None;
    let mut chat_webview: Option<WebView> = None;
    let mut was_alt_space_down = false;
    let mut last_hotkey_poll = Instant::now();
    let root_id = robot_window.id();

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(30));

        if last_hotkey_poll.elapsed() >= Duration::from_millis(30) {
            let alt = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x12) < 0 };
            let space = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x20) < 0 };
            let down = alt && space;
            if down && !was_alt_space_down {
                robot_window.set_visible(true);
                open_chat(target, &mut chat_window, &mut chat_webview);
            }
            was_alt_space_down = down;
            last_hotkey_poll = Instant::now();
        }

        while let Ok(message) = robot_rx.try_recv() {
            if message == "open-chat" {
                open_chat(target, &mut chat_window, &mut chat_webview);
            } else if let Some(delta) = message.strip_prefix("drag:") {
                if let Some((dx, dy)) = delta.split_once(',').and_then(|(x, y)| Some((x.parse::<f64>().ok()?, y.parse::<f64>().ok()?))) {
                    if let Ok(position) = robot_window.outer_position() {
                        let scale = robot_window.scale_factor();
                        robot_window.set_outer_position(LogicalPosition::new(position.x as f64 / scale + dx, position.y as f64 / scale + dy));
                    }
                }
            }
        }

        if let Event::WindowEvent { window_id, event, .. } = event {
            if window_id == root_id {
                if matches!(event, WindowEvent::CloseRequested) {
                    *control_flow = ControlFlow::Exit;
                }
            } else if chat_window.as_ref().map(Window::id) == Some(window_id) {
                if matches!(event, WindowEvent::CloseRequested) {
                    // Dropping both objects tears down the WebView2 controller and window.
                    chat_webview = None;
                    chat_window = None;
                }
            }
        }
    });
}

fn open_chat(
    target: &EventLoopWindowTarget<()>,
    chat_window: &mut Option<Window>,
    chat_webview: &mut Option<WebView>,
) {
    if let Some(window) = chat_window.as_ref() {
        window.set_visible(true);
        window.set_focus();
        return;
    }

    let window = match WindowBuilder::new()
        .with_title("AI Companion — Phase 0")
        .with_inner_size(LogicalSize::new(420.0, 560.0))
        .with_position(LogicalPosition::new(180.0, 140.0))
        .with_min_inner_size(LogicalSize::new(340.0, 420.0))
        .with_always_on_top(true)
        .build(target)
    {
        Ok(window) => window,
        Err(error) => { eprintln!("Could not create chat window: {error}"); return; }
    };

    match WebViewBuilder::new().with_html(CHAT_HTML).build(&window) {
        Ok(webview) => {
            *chat_window = Some(window);
            *chat_webview = Some(webview);
        }
        Err(error) => eprintln!("Could not create chat WebView2: {error}"),
    }
}
