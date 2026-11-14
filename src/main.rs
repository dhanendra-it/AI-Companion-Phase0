
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod backend;
mod chat;
mod keys;
mod screen;

use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
};

use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition},
    event::{Event, WindowEvent},
    event_loop::{
        ControlFlow,
        EventLoopBuilder,
        EventLoopProxy,
        EventLoopWindowTarget,
    },
    platform::windows::{
        EventLoopBuilderExtWindows,
        WindowBuilderExtWindows,
    },
    window::{Window, WindowBuilder},
};

use wry::{PermissionKind, PermissionResponse, WebView, WebViewBuilder};

const ROBOT_HTML: &str = include_str!("../assets/robot.html");
const CHAT_HTML: &str = include_str!("../assets/chat.html");

const ROBOT_WIDTH: f64 = 155.0;
const ROBOT_HEIGHT: f64 = 195.0;
const ROBOT_MARGIN: f64 = 16.0;
const CHAT_WIDTH: f64 = 420.0;
const CHAT_HEIGHT: f64 = 560.0;
const CHAT_GAP: f64 = 8.0;

enum AppEvent {
    GlobalHotkey,
    RobotMessage(String),
    ChatMessage(String),
    BackendOut(backend::Out),
}

#[repr(C)]
struct NativePoint {
    x: i32,
    y: i32,
}

#[repr(C)]
struct NativeMessage {
    hwnd: *mut c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: NativePoint,
}

const HOTKEY_ID: i32 = 1;
const WM_HOTKEY: u32 = 0x0312;
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_NOREPEAT: u32 = 0x4000;
const VK_SPACE: u32 = 0x20;

#[link(name = "user32")]
extern "system" {
    fn RegisterHotKey(
        hwnd: *mut c_void,
        id: i32,
        modifiers: u32,
        key: u32,
    ) -> i32;

    fn UnregisterHotKey(
        hwnd: *mut c_void,
        id: i32,
    ) -> i32;
}

fn register_global_hotkey() -> bool {
    let hwnd = std::ptr::null_mut();

    if unsafe {
        RegisterHotKey(
            hwnd,
            HOTKEY_ID,
            MOD_ALT | MOD_NOREPEAT,
            VK_SPACE,
        )
    } != 0 {
        return true;
    }

    eprintln!("Could not register Alt+Space; trying Ctrl+Alt+Space instead.");

    if unsafe {
        RegisterHotKey(
            hwnd,
            HOTKEY_ID,
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
            VK_SPACE,
        )
    } == 0 {
        eprintln!("Could not register Ctrl+Alt+Space either.");
        false
    } else {
        true
    }
}

fn unregister_global_hotkey() {
    unsafe {
        UnregisterHotKey(
            std::ptr::null_mut(),
            HOTKEY_ID,
        );
    }
}

fn screen_text() -> Result<String, String> {
    let shot = screen::capture_screen()?;
    screen::ocr(&shot)
}

fn screen_jpeg() -> Result<String, String> {
    let shot = screen::capture_screen()?;
    Ok(ai::base64(&screen::jpeg(&shot)?))
}

// Calculate robot position using physical monitor coordinates.
fn robot_start_position(
    target: &EventLoopWindowTarget<AppEvent>,
) -> PhysicalPosition<i32> {
    if let Some(monitor) = target.primary_monitor() {
        let scale = monitor.scale_factor();

        let pos = monitor.position();
        let size = monitor.size();

        // Convert logical dimensions into physical pixels.
        let robot_width =
            (ROBOT_WIDTH * scale).round() as i32;

        let robot_height =
            (ROBOT_HEIGHT * scale).round() as i32;

        let margin =
            (ROBOT_MARGIN * scale).round() as i32;

        // Right side with margin.
        let x = pos.x
            + size.width as i32
            - robot_width
            - margin;

        // Exact vertical center.
        let y = pos.y
            + (size.height as i32 - robot_height) / 2;

        return PhysicalPosition::new(x, y);
    }

    // Fallback position.
    PhysicalPosition::new(1000, 400)
}

fn main() -> wry::Result<()> {
    let mut builder =
        EventLoopBuilder::<AppEvent>::with_user_event();

    let hotkey_proxy =
        Arc::new(Mutex::new(None::<EventLoopProxy<AppEvent>>));

    let hook_proxy = hotkey_proxy.clone();

    builder.with_msg_hook(move |raw| {
        let msg = unsafe {
            &*(raw as *const NativeMessage)
        };

        if msg.message == WM_HOTKEY
            && msg.wparam == HOTKEY_ID as usize
        {
            if let Some(proxy) =
                hook_proxy.lock().unwrap().as_ref().cloned()
            {
                let _ = proxy.send_event(
                    AppEvent::GlobalHotkey,
                );
            }

            true
        } else {
            false
        }
    });

    let event_loop = builder.build();

    let proxy = event_loop.create_proxy();

    *hotkey_proxy.lock().unwrap() = Some(proxy.clone());

    let hotkey_registered = register_global_hotkey();

    let hotkey_name = if hotkey_registered {
        "Alt+Space"
    } else {
        "Ctrl+Alt+Space"
    };

    let emit_proxy = proxy.clone();

    let emit: backend::Emit = Arc::new(move |out| {
        let _ = emit_proxy.send_event(
            AppEvent::BackendOut(out),
        );
    });

    let backend = Arc::new(backend::Backend::new(
        ai::load_config(),
        backend::Deps {
            key_get: keys::get,
            key_set: keys::set,
            screen_text,
            screen_jpeg,
        },
        emit,
        hotkey_name.to_string(),
    ));

    // Robot window: right-center position.
    let robot_position =
        robot_start_position(&event_loop);

    let robot_window = WindowBuilder::new()
        .with_title("AI Companion")
        .with_inner_size(LogicalSize::new(
            ROBOT_WIDTH,
            ROBOT_HEIGHT,
        ))
        .with_position(robot_position)
        .with_decorations(false)
        .with_undecorated_shadow(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_skip_taskbar(true)
        .with_content_protection(true)
        .build(&event_loop)
        .expect("create robot window");

    let robot_proxy = proxy.clone();

    let robot_webview = WebViewBuilder::new()
        .with_html(ROBOT_HTML)
        .with_transparent(true)
        .with_ipc_handler(move |request| {
            let _ = robot_proxy.send_event(
                AppEvent::RobotMessage(
                    request.body().clone(),
                ),
            );
        })
        .build(&robot_window)?;

    let mut chat_window: Option<Window> = None;
    let mut chat_webview: Option<WebView> = None;

    let root_id = robot_window.id();
    let mut chat_ready = false;
    let mut pending_voice_toggle = false;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(AppEvent::GlobalHotkey) => {
                robot_window.set_visible(true);

                toggle_chat(
                    target,
                    &robot_window,
                    &proxy,
                    &mut chat_window,
                    &mut chat_webview,
                );
            }

            Event::UserEvent(AppEvent::RobotMessage(message)) => {
                if message == "open-chat" {
                    open_chat(
                    target,
                    &robot_window,
                    &proxy,
                        &mut chat_window,
                        &mut chat_webview,
                    );
                } else if message == "toggle-chat" {
                    toggle_chat(
                        target,
                        &robot_window,
                        &proxy,
                        &mut chat_window,
                        &mut chat_webview,
                    );
                } else if message == "toggle-call" {
                    open_chat(target, &robot_window, &proxy, &mut chat_window, &mut chat_webview);
                    if chat_ready {
                        if let Some(view) = chat_webview.as_ref() {
                            let _ = view.evaluate_script("window.__voice && window.__voice.toggle()");
                        }
                    } else {
                        pending_voice_toggle = true;
                    }
                } else if message == "quit" {
                    unregister_global_hotkey();
                    *control_flow = ControlFlow::Exit;
                } else if let Some(delta) = message.strip_prefix("drag:") {
                    if let Some((dx, dy)) = delta
                        .split_once(',')
                        .and_then(|(x, y)| {
                            Some((
                                x.parse::<f64>().ok()?,
                                y.parse::<f64>().ok()?,
                            ))
                        })
                    {
                        if let Ok(pos) = robot_window.outer_position() {
                            let scale = robot_window.scale_factor();

                            robot_window.set_outer_position(
                                LogicalPosition::new(
                                    pos.x as f64 / scale + dx,
                                    pos.y as f64 / scale + dy,
                                ),
                            );
                        }
                    }
                }
            }

            Event::UserEvent(AppEvent::ChatMessage(raw)) => {
                let ipc: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
                if ipc["t"] == "voice-state" {
                    let active = ipc["active"].as_bool().unwrap_or(false);
                    let _ = robot_webview.evaluate_script(&format!("window.__setCallActive({active})"));
                } else {
                    if ipc["t"] == "ready" {
                        chat_ready = true;
                        if pending_voice_toggle {
                            pending_voice_toggle = false;
                            if let Some(view) = chat_webview.as_ref() {
                                let _ = view.evaluate_script("window.__voice && window.__voice.toggle()");
                            }
                        }
                    }
                    if let Some(action) = chat::parse_window_action(&raw) {
                        if let Some(window) = chat_window.as_ref() {
                            match action {
                                chat::WindowAction::Minimize => window.set_minimized(true),
                                chat::WindowAction::Maximize => window.set_maximized(!window.is_maximized()),
                                chat::WindowAction::Drag => {
                                    let _ = window.drag_window();
                                }
                            }
                        }
                    } else {
                        backend.handle(&raw);
                    }
                }
            }

            Event::UserEvent(AppEvent::BackendOut(out)) => {
                match out {
                    backend::Out::Js(js) => {
                        if let Some(view) = chat_webview.as_ref() {
                            let _ = view.evaluate_script(&js);
                        }
                    }

                    backend::Out::Overlays(show) => {
                        robot_window.set_visible(show);

                        if let Some(window) = chat_window.as_ref() {
                            window.set_visible(show);
                        }
                    }

                    backend::Out::CloseChat => {
                        backend.reset();
                        chat_ready = false;
                        pending_voice_toggle = false;
                        let _ = robot_webview.evaluate_script("window.__setCallActive(false)");
                        chat_webview = None;
                        chat_window = None;
                    }
                }
            }

            Event::WindowEvent {
                window_id,
                event: WindowEvent::Moved(_),
                ..
            } if window_id == root_id => {
                if let Some(window) = chat_window.as_ref() {
                    window.set_outer_position(chat_position(target, &robot_window));
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if window_id == root_id {
                    unregister_global_hotkey();
                    *control_flow = ControlFlow::Exit;
                } else if chat_window
                    .as_ref()
                    .map(Window::id) == Some(window_id)
                {
                    backend.reset();
                    chat_ready = false;
                    pending_voice_toggle = false;
                    let _ = robot_webview.evaluate_script("window.__setCallActive(false)");
                    chat_webview = None;
                    chat_window = None;
                }
            }

            _ => {}
        }
    });
}

fn toggle_chat(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    chat_window: &mut Option<Window>,
    chat_webview: &mut Option<WebView>,
) {
    if let Some(window) = chat_window.as_ref() {
        let visible = window.is_visible();

        window.set_visible(!visible);

        if !visible {
            window.set_outer_position(chat_position(target, robot_window));
            window.set_focus();
        }

        return;
    }

    open_chat(
        target,
        robot_window,
        proxy,
        chat_window,
        chat_webview,
    );
}

fn chat_position(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
) -> PhysicalPosition<i32> {
    let robot_pos = robot_window.outer_position().unwrap_or_else(|_| robot_start_position(target));
    let robot_size = robot_window.outer_size();
    let center_x = robot_pos.x + robot_size.width as i32 / 2;
    let center_y = robot_pos.y + robot_size.height as i32 / 2;
    let monitor = target
        .available_monitors()
        .find(|monitor| {
            let pos = monitor.position();
            let size = monitor.size();
            center_x >= pos.x
                && center_y >= pos.y
                && center_x < pos.x + size.width as i32
                && center_y < pos.y + size.height as i32
        })
        .or_else(|| target.primary_monitor());
    let Some(monitor) = monitor else {
        return PhysicalPosition::new(robot_pos.x - 420, robot_pos.y - 400);
    };

    let scale = robot_window.scale_factor();
    let pos = monitor.position();
    let size = monitor.size();
    let margin = (ROBOT_MARGIN * scale).round() as i32;
    let gap = (CHAT_GAP * scale).round() as i32;
    let chat_w = (CHAT_WIDTH * scale).round() as i32;
    let chat_h = (CHAT_HEIGHT * scale).round() as i32;
    let robot_w = robot_size.width as i32;
    let robot_h = robot_size.height as i32;
    let left = pos.x + margin;
    let top = pos.y + margin;
    let right = pos.x + size.width as i32 - margin;
    let bottom = pos.y + size.height as i32 - margin;
    let y_bottom_aligned = robot_pos.y + robot_h - chat_h;
    let candidates = [
        (robot_pos.x - chat_w - gap, y_bottom_aligned),
        (robot_pos.x + robot_w + gap, y_bottom_aligned),
        (robot_pos.x + robot_w - chat_w, robot_pos.y - chat_h - gap),
        (robot_pos.x + robot_w - chat_w, robot_pos.y + robot_h + gap),
    ];
    let fits = |(x, y): (i32, i32)| {
        x >= left && y >= top && x + chat_w <= right && y + chat_h <= bottom
    };
    let chosen = candidates.iter().copied().find(|candidate| fits(*candidate)).unwrap_or(candidates[0]);
    let max_x = (right - chat_w).max(left);
    let max_y = (bottom - chat_h).max(top);
    PhysicalPosition::new(chosen.0.clamp(left, max_x), chosen.1.clamp(top, max_y))
}
fn open_chat(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    chat_window: &mut Option<Window>,
    chat_webview: &mut Option<WebView>,
) {
    if let Some(window) = chat_window.as_ref() {
        window.set_outer_position(chat_position(target, robot_window));
        window.set_visible(true);
        window.set_focus();
        return;
    }

    let window = match WindowBuilder::new()
        .with_title("Aalu — AI Companion")
        .with_inner_size(LogicalSize::new(CHAT_WIDTH, CHAT_HEIGHT))
        .with_position(chat_position(target, robot_window))
        .with_min_inner_size(LogicalSize::new(340.0, 420.0))
        .with_always_on_top(true)
        .with_decorations(false)
        .with_resizable(true)
        .with_content_protection(true)
        .build(target)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Could not create chat window: {e}");
            return;
        }
    };

    let chat_proxy = proxy.clone();

    match WebViewBuilder::new()
        .with_html(CHAT_HTML)
        .with_permission_handler(|kind| match kind {
            PermissionKind::Microphone => PermissionResponse::Default,
            _ => PermissionResponse::Default,
        })
        .with_ipc_handler(move |request| {
            let _ = chat_proxy.send_event(
                AppEvent::ChatMessage(
                    request.body().clone(),
                ),
            );
        })
        .build(&window)
    {
        Ok(view) => {
            *chat_window = Some(window);
            *chat_webview = Some(view);
        }

        Err(e) => {
            eprintln!("Could not create chat WebView2: {e}");
        }
    }
}
