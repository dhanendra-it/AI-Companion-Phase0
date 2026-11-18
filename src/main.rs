
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod app_icon;
mod backend;
mod bridge;
mod chat;
mod keys;
mod scheduler;
mod screen;

use std::{
    borrow::Cow,
    process::Command,
    ffi::c_void,
    sync::{Arc, Mutex},
};

use std::os::windows::process::CommandExt;

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

use wry::{PermissionKind, PermissionResponse, WebView, WebViewBuilder, WebViewBuilderExtWindows};

const ROBOT_HTML: &str = include_str!("../assets/robot.html");
const CHAT_HTML: &str = include_str!("../assets/chat.html");
const SCHEDULER_HTML: &str = include_str!("../assets/scheduler.html");

const ROBOT_WIDTH: f64 = 155.0;
const ROBOT_HEIGHT: f64 = 195.0;
const ROBOT_MARGIN: f64 = 16.0;
const CHAT_WIDTH: f64 = 420.0;
const CHAT_HEIGHT: f64 = 560.0;
const CHAT_GAP: f64 = 8.0;
const SCHEDULER_WIDTH: f64 = 580.0;
const SCHEDULER_HEIGHT: f64 = 480.0;
const REMINDER_EXPAND_W: f64 = 145.0;
const REMINDER_EXPAND_H: f64 = 70.0;

enum AppEvent {
    ToggleSchedulerHotkey,
    ToggleChatHotkey,
    ToggleRobotHotkey,
    RobotMessage(String),
    ChatMessage(String),
    BackendOut(backend::Out),
    Reminder(Vec<String>),
    SchedulerMessage(String),
    Bridge(bridge::Event),
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

const HOTKEY_CHAT_ID: i32 = 1;
const HOTKEY_SCHEDULER_ID: i32 = 2;
const HOTKEY_ROBOT_ID: i32 = 3;
const WM_HOTKEY: u32 = 0x0312;
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_NOREPEAT: u32 = 0x4000;
const VK_SPACE: u32 = 0x20;
const VK_UP: u32 = 0x26;
const VK_RCONTROL: i32 = 0xA3;

#[link(name = "user32")]
extern "system" {
    fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
    fn GetAsyncKeyState(vkey: i32) -> i16;
}

fn register_global_hotkeys() -> (bool, bool, bool) {
    let hwnd = std::ptr::null_mut();
    let chat = unsafe { RegisterHotKey(hwnd, HOTKEY_CHAT_ID, MOD_ALT | MOD_NOREPEAT, VK_SPACE) } != 0;
    let scheduler = unsafe { RegisterHotKey(hwnd, HOTKEY_SCHEDULER_ID, MOD_CONTROL | MOD_NOREPEAT, VK_SPACE) } != 0;
    let robot = unsafe { RegisterHotKey(hwnd, HOTKEY_ROBOT_ID, MOD_CONTROL | MOD_NOREPEAT, VK_UP) } != 0;
    if !chat { eprintln!("Could not register Alt+Space for chat."); }
    if !scheduler { eprintln!("Could not register Ctrl+Space for scheduler."); }
    if !robot { eprintln!("Could not register Right Ctrl+Up for robot visibility."); }
    (chat, scheduler, robot)
}

fn unregister_global_hotkeys() {
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_CHAT_ID);
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_SCHEDULER_ID);
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ROBOT_ID);
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
        let msg = unsafe { &*(raw as *const NativeMessage) };
        if msg.message == WM_HOTKEY {
            let event = match msg.wparam as i32 {
                HOTKEY_CHAT_ID => Some(AppEvent::ToggleChatHotkey),
                HOTKEY_SCHEDULER_ID if unsafe { GetAsyncKeyState(VK_RCONTROL) as u16 & 0x8000 != 0 } => Some(AppEvent::ToggleSchedulerHotkey),
                HOTKEY_SCHEDULER_ID => None,
                HOTKEY_ROBOT_ID if unsafe { GetAsyncKeyState(VK_RCONTROL) as u16 & 0x8000 != 0 } => Some(AppEvent::ToggleRobotHotkey),
                HOTKEY_ROBOT_ID => None,
                _ => return false,
            };
            if let (Some(event), Some(proxy)) = (event, hook_proxy.lock().unwrap().as_ref().cloned()) {
                let _ = proxy.send_event(event);
            }
            return true;
        }
        false
    });

    let event_loop = builder.build();

    let proxy = event_loop.create_proxy();

    *hotkey_proxy.lock().unwrap() = Some(proxy.clone());

    let (chat_hotkey_available, _scheduler_hotkey_available, _robot_hotkey_available) = register_global_hotkeys();

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
        if chat_hotkey_available { "Alt+Space".to_string() } else { String::new() },
    ));

    enable_startup();

    // DeskBridge: keyboard + mouse to the Android tablet (Right Ctrl + Right Shift).
    let bridge_proxy = proxy.clone();
    bridge::start(move |event| {
        let _ = bridge_proxy.send_event(AppEvent::Bridge(event));
    });

    // Robot window: right-center position.
    let robot_position =
        robot_start_position(&event_loop);

    let robot_window = WindowBuilder::new()
        .with_title("Aalu")
        .with_window_icon(app_icon::window_icon())
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
        .with_visible(false)
        .build(&event_loop)
        .expect("create robot window");

    let robot_webview = {
        let mut created = None;
        let mut last_error = None;
        for attempt in 1..=3 {
            let robot_proxy = proxy.clone();
            match WebViewBuilder::new()
                .with_autoplay(true)
                .with_html(ROBOT_HTML)
                .with_transparent(true)
                .with_ipc_handler(move |request| {
                    let _ = robot_proxy.send_event(AppEvent::RobotMessage(request.body().clone()));
                })
                .build(&robot_window)
            {
                Ok(view) => { created = Some(view); break; }
                Err(error) => {
                    eprintln!("Could not create robot WebView2 (attempt {attempt}/3): {error}");
                    last_error = Some(error);
                    if attempt < 3 { std::thread::sleep(std::time::Duration::from_millis(500)); }
                }
            }
        }
        match created {
            Some(view) => view,
            None => return Err(last_error.expect("robot WebView creation failed")),
        }
    };
    robot_window.set_visible(true);
    let mut chat_window: Option<Window> = None;
    let mut chat_webview: Option<WebView> = None;

    let root_id = robot_window.id();
    let mut chat_ready = false;
    let mut pending_voice_toggle = false;
    let mut voice_active = false;

    let schedule = scheduler::load_schedule();
    let shared_sched = scheduler::new_shared_schedule(schedule);
    let sched_timer_proxy = proxy.clone();
    let _timer_thread = scheduler::start_scheduler_thread(shared_sched.clone(), move |lines| {
        let _ = sched_timer_proxy.send_event(AppEvent::Reminder(lines));
    });

    let mut is_reminder_active = false;
    let mut scheduler_window: Option<Window> = None;
    let mut scheduler_webview: Option<WebView> = None;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(AppEvent::ToggleRobotHotkey) => {
                let show = !robot_window.is_visible();
                robot_window.set_visible(show);
                let _ = robot_webview.evaluate_script(&format!("window.__setAppVisible({show})"));
                if !show {
                    if let Some(view) = chat_webview.as_ref() {
                        let _ = view.evaluate_script("window.__voice && window.__voice.stop()");
                    }
                    if let Some(window) = chat_window.as_ref() { window.set_visible(false); }
                    if let Some(window) = scheduler_window.as_ref() { window.set_visible(false); }
                }
            }

            Event::UserEvent(AppEvent::ToggleSchedulerHotkey) => {
                robot_window.set_visible(true);
                let _ = robot_webview.evaluate_script("window.__setAppVisible(true)");

                toggle_scheduler(
                    target,
                    &robot_window,
                    &proxy,
                    &mut scheduler_window,
                    &mut scheduler_webview,
                    is_reminder_active,
                );
            }

            Event::UserEvent(AppEvent::ToggleChatHotkey) => {
                robot_window.set_visible(true);
                let _ = robot_webview.evaluate_script("window.__setAppVisible(true)");

                toggle_chat(
                    target,
                    &robot_window,
                    &proxy,
                    &mut chat_window,
                    &mut chat_webview,
                    is_reminder_active,
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
                        is_reminder_active,
                    );
                } else if message == "toggle-chat" {
                    toggle_chat(
                        target,
                        &robot_window,
                        &proxy,
                        &mut chat_window,
                        &mut chat_webview,
                        is_reminder_active,
                    );
                } else if message == "toggle-call" {
                    if voice_active {
                        if let Some(view) = chat_webview.as_ref() {
                            let _ = view.evaluate_script("window.__voice && window.__voice.toggle()");
                        }
                        return;
                    }
                    open_chat(
                        target,
                        &robot_window,
                        &proxy,
                        &mut chat_window,
                        &mut chat_webview,
                        is_reminder_active,
                    );
                    if chat_ready {
                        if let Some(view) = chat_webview.as_ref() {
                            let _ = view.evaluate_script("window.__voice && window.__voice.toggle()");
                        }
                    } else {
                        pending_voice_toggle = true;
                    }
                } else if message == "toggle-scheduler" || message == "open-scheduler" {
                    toggle_scheduler(
                        target,
                        &robot_window,
                        &proxy,
                        &mut scheduler_window,
                        &mut scheduler_webview,
                        is_reminder_active,
                    );
                } else if message == "reminder:hide" {
                    if is_reminder_active {
                        is_reminder_active = false;
                        shrink_robot_window(&robot_window);
                    }
                } else if message == "quit" {
                    unregister_global_hotkeys();
                    *control_flow = ControlFlow::Exit;
                } else if let Some(delta) = message.strip_prefix("drag:") {
                    if let Some((dx, dy)) = delta.split_once(',').and_then(|(x, y)| {
                        Some((x.parse::<f64>().ok()?, y.parse::<f64>().ok()?))
                    }) {
                        if let Ok(pos) = robot_window.outer_position() {
                            let scale = robot_window.scale_factor();

                            robot_window.set_outer_position(LogicalPosition::new(
                                pos.x as f64 / scale + dx,
                                pos.y as f64 / scale + dy,
                            ));
                        }
                    }
                }
            }

            Event::UserEvent(AppEvent::ChatMessage(raw)) => {
                let ipc: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
                if ipc["t"] == "voice-state" {
                    let active = ipc["active"].as_bool().unwrap_or(false);
                    voice_active = active;
                    let _ = robot_webview.evaluate_script(&format!("window.__setCallActive({active})"));
                } else if ipc["t"] == "voice-failed" {
                    if let Some(window) = chat_window.as_ref() { window.set_visible(true); window.set_focus(); }
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
                                chat::WindowAction::Maximize => {
                                    window.set_maximized(!window.is_maximized())
                                }
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

            Event::UserEvent(AppEvent::Reminder(lines)) => {
                if !is_reminder_active {
                    is_reminder_active = true;
                    grow_robot_window(&robot_window);
                }
                let js_array = serde_json::to_string(&lines).unwrap_or_else(|_| "[]".into());
                let _ = robot_webview.evaluate_script(&format!("window.__reminder({js_array})"));
            }

            Event::UserEvent(AppEvent::SchedulerMessage(raw)) => {
                let ipc: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
                match ipc["t"].as_str() {
                    Some("load") => {
                        let current = shared_sched.0.lock().unwrap().clone();
                        let json_str =
                            serde_json::to_string(&current).unwrap_or_else(|_| "null".into());
                        if let Some(view) = scheduler_webview.as_ref() {
                            let _ = view.evaluate_script(&format!("window.__sched.load({json_str})"));
                        }
                    }
                    Some("save") => {
                        let data = ipc.get("data");
                        let mut result: Result<(), String> = Err("Missing data".into());
                        if let Some(data_val) = data {
                            match serde_json::from_value::<scheduler::Schedule>(data_val.clone()) {
                                Ok(mut new_sched) => {
                                    match scheduler::save_schedule(&mut new_sched) {
                                        Ok(()) => {
                                            *shared_sched.0.lock().unwrap() = new_sched;
                                            shared_sched.1.notify_one();
                                            result = Ok(());
                                        }
                                        Err(e) => {
                                            result = Err(e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    result = Err(format!("Invalid format: {e}"));
                                }
                            }
                        }
                        if let Some(view) = scheduler_webview.as_ref() {
                            match result {
                                Ok(()) => {
                                    let _ = view.evaluate_script("window.__sched.saveResult(true, null)");
                                }
                                Err(err) => {
                                    let err_js = serde_json::to_string(&err)
                                        .unwrap_or_else(|_| "\"Save error\"".into());
                                    let _ = view.evaluate_script(&format!(
                                        "window.__sched.saveResult(false, {err_js})"
                                    ));
                                }
                            }
                        }
                    }
                    Some("close") => {
                        scheduler_webview = None;
                        scheduler_window = None;
                    }
                    Some("window") => {
                        if let Some(action) = chat::parse_window_action(&raw) {
                            if let Some(window) = scheduler_window.as_ref() {
                                match action {
                                    chat::WindowAction::Minimize => window.set_minimized(true),
                                    chat::WindowAction::Maximize => {
                                        window.set_maximized(!window.is_maximized())
                                    }
                                    chat::WindowAction::Drag => {
                                        let _ = window.drag_window();
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            Event::UserEvent(AppEvent::Bridge(event)) => {
                let js = match event {
                    bridge::Event::Mode(on) => format!("window.__setTabletMode && window.__setTabletMode({on})"),
                    bridge::Event::Refused(why) => {
                        let text = serde_json::to_string(&why).unwrap_or_else(|_| "\"\"".into());
                        format!("window.__tabletNotice && window.__tabletNotice({text})")
                    }
                    // Connection changes are only worth a message when they are the reason the
                    // shortcut would not work; the robot shows that when the shortcut is pressed.
                    bridge::Event::Link { .. } => String::new(),
                };
                if !js.is_empty() {
                    let _ = robot_webview.evaluate_script(&js);
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
                        let _ = robot_webview.evaluate_script(&format!("window.__setAppVisible({show})"));

                        if let Some(window) = chat_window.as_ref() {
                            window.set_visible(show);
                        }
                        if let Some(window) = scheduler_window.as_ref() {
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
                    window.set_outer_position(chat_position(
                        target,
                        &robot_window,
                        is_reminder_active,
                    ));
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if window_id == root_id {
                    unregister_global_hotkeys();
                    *control_flow = ControlFlow::Exit;
                } else if chat_window.as_ref().map(Window::id) == Some(window_id) {
                    backend.reset();
                    chat_ready = false;
                    pending_voice_toggle = false;
                    let _ = robot_webview.evaluate_script("window.__setCallActive(false)");
                    chat_webview = None;
                    chat_window = None;
                } else if scheduler_window.as_ref().map(Window::id) == Some(window_id) {
                    scheduler_webview = None;
                    scheduler_window = None;
                }
            }

            _ => {}
        }
    });
}

fn grow_robot_window(window: &Window) {
    let scale = window.scale_factor();
    if let Ok(pos) = window.outer_position() {
        let dx = (REMINDER_EXPAND_W * scale).round() as i32;
        let dy = (REMINDER_EXPAND_H * scale).round() as i32;
        window.set_outer_position(PhysicalPosition::new(pos.x - dx, pos.y - dy));
        window.set_inner_size(LogicalSize::new(
            ROBOT_WIDTH + REMINDER_EXPAND_W,
            ROBOT_HEIGHT + REMINDER_EXPAND_H,
        ));
    }
}

fn shrink_robot_window(window: &Window) {
    let scale = window.scale_factor();
    if let Ok(pos) = window.outer_position() {
        let dx = (REMINDER_EXPAND_W * scale).round() as i32;
        let dy = (REMINDER_EXPAND_H * scale).round() as i32;
        window.set_inner_size(LogicalSize::new(ROBOT_WIDTH, ROBOT_HEIGHT));
        window.set_outer_position(PhysicalPosition::new(pos.x + dx, pos.y + dy));
    }
}

fn toggle_chat(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    chat_window: &mut Option<Window>,
    chat_webview: &mut Option<WebView>,
    is_reminder_active: bool,
) {
    if let Some(window) = chat_window.as_ref() {
        let visible = window.is_visible();

        window.set_visible(!visible);

        if !visible {
            window.set_outer_position(chat_position(target, robot_window, is_reminder_active));
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
        is_reminder_active,
    );
}

fn toggle_scheduler(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    scheduler_window: &mut Option<Window>,
    scheduler_webview: &mut Option<WebView>,
    is_reminder_active: bool,
) {
    if let Some(window) = scheduler_window.as_ref() {
        if window.is_visible() {
            *scheduler_webview = None;
            *scheduler_window = None;
            return;
        } else {
            window.set_outer_position(scheduler_position(
                target,
                robot_window,
                is_reminder_active,
            ));
            window.set_visible(true);
            window.set_focus();
            return;
        }
    }

    open_scheduler(
        target,
        robot_window,
        proxy,
        scheduler_window,
        scheduler_webview,
        is_reminder_active,
    );
}

fn aligned_window_position(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    window_logical_w: f64,
    window_logical_h: f64,
    is_reminder_active: bool,
) -> PhysicalPosition<i32> {
    let mut robot_pos = robot_window
        .outer_position()
        .unwrap_or_else(|_| robot_start_position(target));
    let scale = robot_window.scale_factor();

    if is_reminder_active {
        robot_pos.x += (REMINDER_EXPAND_W * scale).round() as i32;
        robot_pos.y += (REMINDER_EXPAND_H * scale).round() as i32;
    }

    let robot_w = (ROBOT_WIDTH * scale).round() as i32;
    let robot_h = (ROBOT_HEIGHT * scale).round() as i32;
    let center_x = robot_pos.x + robot_w / 2;
    let center_y = robot_pos.y + robot_h / 2;

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

    let win_w = (window_logical_w * scale).round() as i32;
    let win_h = (window_logical_h * scale).round() as i32;

    let Some(monitor) = monitor else {
        return PhysicalPosition::new(robot_pos.x - win_w, robot_pos.y - win_h);
    };

    let pos = monitor.position();
    let size = monitor.size();
    let margin = (ROBOT_MARGIN * scale).round() as i32;
    let gap = (CHAT_GAP * scale).round() as i32;

    let left = pos.x + margin;
    let top = pos.y + margin;
    let right = pos.x + size.width as i32 - margin;
    let bottom = pos.y + size.height as i32 - margin;
    let y_bottom_aligned = robot_pos.y + robot_h - win_h;

    let candidates = [
        (robot_pos.x - win_w - gap, y_bottom_aligned),
        (robot_pos.x + robot_w + gap, y_bottom_aligned),
        (robot_pos.x + robot_w - win_w, robot_pos.y - win_h - gap),
        (robot_pos.x + robot_w - win_w, robot_pos.y + robot_h + gap),
    ];
    let fits = |(x, y): (i32, i32)| {
        x >= left && y >= top && x + win_w <= right && y + win_h <= bottom
    };
    let chosen = candidates
        .iter()
        .copied()
        .find(|candidate| fits(*candidate))
        .unwrap_or(candidates[0]);
    let max_x = (right - win_w).max(left);
    let max_y = (bottom - win_h).max(top);
    PhysicalPosition::new(chosen.0.clamp(left, max_x), chosen.1.clamp(top, max_y))
}

fn chat_position(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    is_reminder_active: bool,
) -> PhysicalPosition<i32> {
    aligned_window_position(target, robot_window, CHAT_WIDTH, CHAT_HEIGHT, is_reminder_active)
}

fn scheduler_position(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    is_reminder_active: bool,
) -> PhysicalPosition<i32> {
    aligned_window_position(
        target,
        robot_window,
        SCHEDULER_WIDTH,
        SCHEDULER_HEIGHT,
        is_reminder_active,
    )
}

fn open_chat(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    chat_window: &mut Option<Window>,
    chat_webview: &mut Option<WebView>,
    is_reminder_active: bool,
) {
    if let Some(window) = chat_window.as_ref() {
        window.set_outer_position(chat_position(target, robot_window, is_reminder_active));
        window.set_visible(true);
        window.set_focus();
        return;
    }

    let window = match WindowBuilder::new()
        .with_title("Aalu")
        .with_window_icon(app_icon::window_icon())
        .with_inner_size(LogicalSize::new(CHAT_WIDTH, CHAT_HEIGHT))
        .with_position(chat_position(target, robot_window, is_reminder_active))
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
        .with_profile_name("AaluVoice")
        .with_https_scheme(true)
        .with_custom_protocol("aalu".into(), |_webview_id, _request| {
            wry::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Cow::Borrowed(CHAT_HTML.as_bytes()))
                .expect("valid embedded chat response")
        })
        .with_url("aalu://chat/index.html")
        .with_permission_handler(|kind| match kind {
            PermissionKind::Microphone => PermissionResponse::Allow,
            _ => PermissionResponse::Default,
        })
        .with_ipc_handler(move |request| {
            let _ = chat_proxy.send_event(AppEvent::ChatMessage(request.body().clone()));
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

fn open_scheduler(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    scheduler_window: &mut Option<Window>,
    scheduler_webview: &mut Option<WebView>,
    is_reminder_active: bool,
) {
    if let Some(window) = scheduler_window.as_ref() {
        window.set_outer_position(scheduler_position(
            target,
            robot_window,
            is_reminder_active,
        ));
        window.set_visible(true);
        window.set_focus();
        return;
    }

    let window = match WindowBuilder::new()
        .with_title("Aalu — Daily Scheduler")
        .with_window_icon(app_icon::window_icon())
        .with_inner_size(LogicalSize::new(SCHEDULER_WIDTH, SCHEDULER_HEIGHT))
        .with_position(scheduler_position(
            target,
            robot_window,
            is_reminder_active,
        ))
        .with_min_inner_size(LogicalSize::new(480.0, 400.0))
        .with_always_on_top(true)
        .with_decorations(false)
        .with_resizable(true)
        .with_content_protection(true)
        .build(target)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Could not create scheduler window: {e}");
            return;
        }
    };

    let sched_proxy = proxy.clone();

    match WebViewBuilder::new()
        .with_https_scheme(true)
        .with_custom_protocol("aalu".into(), |_webview_id, _request| {
            wry::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Cow::Borrowed(SCHEDULER_HTML.as_bytes()))
                .expect("valid embedded scheduler response")
        })
        .with_url("aalu://scheduler/index.html")
        .with_ipc_handler(move |request| {
            let _ = sched_proxy.send_event(AppEvent::SchedulerMessage(request.body().clone()));
        })
        .build(&window)
    {
        Ok(view) => {
            *scheduler_window = Some(window);
            *scheduler_webview = Some(view);
        }

        Err(e) => {
            eprintln!("Could not create scheduler WebView2: {e}");
        }
    }
}


fn enable_startup() {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = format!("\"{}\"", exe.display());
    match Command::new("reg.exe")
        .args(["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "Aalu", "/t", "REG_SZ", "/d", &exe, "/f"])
        .creation_flags(0x08000000)
        .status()
    {
        Ok(status) if !status.success() => eprintln!("Could not enable Aalu at Windows sign-in."),
        Err(error) => eprintln!("Could not enable Aalu at Windows sign-in: {error}"),
        _ => {}
    }
}