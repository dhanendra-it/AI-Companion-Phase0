
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
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering as AtomicOrdering}},
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
        WindowExtWindows,
    },
    window::{Window, WindowBuilder},
};

use wry::{PermissionKind, PermissionResponse, WebView, WebViewBuilder, WebViewBuilderExtWindows};

const ROBOT_HTML: &str = include_str!("../assets/robot.html");
const CHAT_HTML: &str = include_str!("../assets/chat.html");
const SCHEDULER_HTML: &str = include_str!("../assets/scheduler.html");
const TABLET_STATUS_HTML: &str = include_str!("../assets/tablet-status.html");
const PERSONAL_TOOLBAR_HTML: &str = include_str!("../assets/personal-toolbar.html");

const ROBOT_WIDTH: f64 = 132.0;
const ROBOT_HEIGHT: f64 = 166.0;
const ROBOT_MARGIN: f64 = 16.0;
const CHAT_WIDTH: f64 = 420.0;
const CHAT_HEIGHT: f64 = 560.0;
const CHAT_GAP: f64 = 8.0;
const SCHEDULER_WIDTH: f64 = 720.0;
const SCHEDULER_HEIGHT: f64 = 680.0;
const TABLET_STATUS_WIDTH: f64 = 310.0;
const TABLET_STATUS_HEIGHT: f64 = 176.0;
const TOOLBAR_WIDTH: f64 = 500.0;
const TOOLBAR_HEIGHT: f64 = 500.0;
const REMINDER_EXPAND_W: f64 = 190.0;
const REMINDER_EXPAND_H: f64 = 70.0;

enum AppEvent {
    ToggleSchedulerHotkey,
    ToggleChatHotkey,
    ToggleRobotHotkey,
    ToggleToolbar,
    ToolbarMessage(String),
    RobotMessage(String),
    ChatMessage(String),
    BackendOut(backend::Out),
    Reminder(Vec<String>),
    SchedulerMessage(String),
    Bridge(bridge::Event),
    TabletStatusMessage(String),
    HideTabletStatus(u64),
    KeepOnTop,
    ToolbarFocusCheck,
    ToolbarPicked(String),
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
const VK_XBUTTON2: i32 = 0x06;
const ERROR_ALREADY_EXISTS: u32 = 183;

// Controls screenshot visibility of the robot window only. Visible by default.
static ROBOT_VISIBLE_IN_CAPTURE: AtomicBool = AtomicBool::new(true);
// When true the robot makes no sounds (chimes and reminder beeps). Sound is on by default.
static ROBOT_MUTED: AtomicBool = AtomicBool::new(false);
// Separate switch for the scheduler reminder alarm sound (the visual reminder still shows).
static REMINDER_MUTED: AtomicBool = AtomicBool::new(false);

#[link(name = "user32")]
extern "system" {
    fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
    fn GetAsyncKeyState(vkey: i32) -> i16;
    fn GetCursorPos(point: *mut NativePoint) -> i32;
    fn GetForegroundWindow() -> *mut c_void;
    fn SetWindowDisplayAffinity(hwnd: *mut c_void, affinity: u32) -> i32;
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, memory: *mut c_void) -> *mut c_void;
    fn CloseClipboard() -> i32;
}

const WDA_NONE: u32 = 0x0;
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_command: i32,
    ) -> *mut c_void;
}

/// True when the mouse cursor is inside the window's outer rectangle.
fn cursor_over_window(window: &Window) -> bool {
    let mut point = NativePoint { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } == 0 { return false; }
    let (Ok(pos), size) = (window.outer_position(), window.outer_size()) else { return false; };
    point.x >= pos.x && point.y >= pos.y
        && point.x < pos.x + size.width as i32
        && point.y < pos.y + size.height as i32
}

const CAPTURE_SETTING_KEY: &str = r"HKCU\Software\Aalu";

fn load_robot_capture_setting() -> bool {
    let output = Command::new("reg.exe")
        .args(["query", CAPTURE_SETTING_KEY, "/v", "RobotVisibleInCapture"])
        .creation_flags(0x0800_0000)
        .output();
    let Ok(output) = output else { return true };
    if !output.status.success() { return true; }
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.split_whitespace().last().map(|value| value != "0x0").unwrap_or(true)
}

fn save_robot_capture_setting(visible: bool) {
    let value = if visible { "1" } else { "0" };
    let _ = Command::new("reg.exe")
        .args(["add", CAPTURE_SETTING_KEY, "/v", "RobotVisibleInCapture", "/t", "REG_DWORD", "/d", value, "/f"])
        .creation_flags(0x0800_0000)
        .output();
}

fn load_robot_muted_setting() -> bool { load_bool_setting("RobotSoundMuted") }
fn load_reminder_muted_setting() -> bool { load_bool_setting("ReminderSoundMuted") }
fn save_robot_muted_setting(muted: bool) { save_bool_setting("RobotSoundMuted", muted) }
fn save_reminder_muted_setting(muted: bool) { save_bool_setting("ReminderSoundMuted", muted) }

fn load_bool_setting(name: &str) -> bool {
    let output = Command::new("reg.exe")
        .args(["query", CAPTURE_SETTING_KEY, "/v", name])
        .creation_flags(0x0800_0000)
        .output();
    let Ok(output) = output else { return false };
    if !output.status.success() { return false; }
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.split_whitespace().last().map(|value| value != "0x0").unwrap_or(false)
}

fn save_bool_setting(name: &str, muted: bool) {
    let value = if muted { "1" } else { "0" };
    let _ = Command::new("reg.exe")
        .args(["add", CAPTURE_SETTING_KEY, "/v", name, "/t", "REG_DWORD", "/d", value, "/f"])
        .creation_flags(0x0800_0000)
        .output();
}

/// Apply screenshot affinity to the robot HWND only. Chat, scheduler and toolbar are untouched.
fn apply_robot_capture_affinity(window: &Window) {
    let affinity = if ROBOT_VISIBLE_IN_CAPTURE.load(AtomicOrdering::Relaxed) { WDA_NONE } else { WDA_EXCLUDEFROMCAPTURE };
    unsafe { SetWindowDisplayAffinity(window.hwnd() as *mut c_void, affinity); }
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(memory: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(memory: *mut c_void) -> i32;
    fn GlobalFree(memory: *mut c_void) -> *mut c_void;
}

/// Keep one Aalu process per Windows session so global hotkeys and input hooks cannot compete.
fn acquire_single_instance() -> bool {
    let name: Vec<u16> = "Local\\Aalu.Companion.SingleInstance\0".encode_utf16().collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if handle.is_null() {
            eprintln!("Could not create Aalu's single-instance mutex ({}).", GetLastError());
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return false;
        }
        // Intentionally keep this handle open until process exit.
        true
    }
}

/// Open a file, folder, application shortcut, or URL through the Windows Shell.
/// ShellExecuteW correctly handles .lnk files, executables, folders and web URLs.
fn shell_open(target: &str) -> Result<(), String> {
    let target = target.trim().trim_matches('"');
    if target.is_empty() { return Err("No target was provided".into()); }
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let target: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(), operation.as_ptr(), target.as_ptr(),
            std::ptr::null(), std::ptr::null(), 1,
        )
    };
    let code = result as isize;
    if code <= 32 { Err(format!("Windows could not open this item (ShellExecute code {code})")) }
    else { Ok(()) }
}

/// Write Unicode text directly to the Windows clipboard (CF_UNICODETEXT).
fn set_clipboard_text(text: &str) -> Result<(), String> {
    let clean = text.replace('\0', "�");
    let wide: Vec<u16> = clean.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() { return Err("Could not allocate clipboard memory".into()); }
        let destination = GlobalLock(memory) as *mut u16;
        if destination.is_null() {
            GlobalFree(memory);
            return Err("Could not lock clipboard memory".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), destination, wide.len());
        GlobalUnlock(memory);
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            GlobalFree(memory);
            return Err("Windows clipboard is busy; try again".into());
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            GlobalFree(memory);
            return Err("Could not clear the Windows clipboard".into());
        }
        if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
            CloseClipboard();
            GlobalFree(memory);
            return Err("Could not write text to the Windows clipboard".into());
        }
        // Windows owns the movable memory after SetClipboardData succeeds.
        CloseClipboard();
    }
    Ok(())
}

fn notify_toolbar_action(webview_slot: &Option<WebView>, result: Result<(), String>, success_message: &str, close_after: bool) {
    let (ok, message) = match result {
        Ok(()) => (true, success_message.to_string()),
        Err(error) => (false, error),
    };
    let message = serde_json::to_string(&message).unwrap_or_else(|_| "\"Toolbar action failed\"".into());
    if let Some(view) = webview_slot.as_ref() {
        let _ = view.evaluate_script(&format!("window.__actionResult && window.__actionResult({ok}, {message}, {close_after})"));
    }
}

fn register_global_hotkeys() -> (bool, bool, bool) {
    let hwnd = std::ptr::null_mut();
    let chat = unsafe { RegisterHotKey(hwnd, HOTKEY_CHAT_ID, MOD_ALT | MOD_NOREPEAT, VK_SPACE) } != 0;
    let scheduler = unsafe { RegisterHotKey(hwnd, HOTKEY_SCHEDULER_ID, MOD_CONTROL | MOD_NOREPEAT, VK_SPACE) } != 0;
    let robot = unsafe { RegisterHotKey(hwnd, HOTKEY_ROBOT_ID, MOD_CONTROL | MOD_NOREPEAT, VK_UP) } != 0;
    if !chat { eprintln!("Could not register Alt+Space for chat."); }
    if !scheduler { eprintln!("Could not register Ctrl+Space for scheduler."); }
    if !robot { eprintln!("Could not register Right Ctrl+Up for robot visibility."); }
    bridge::log(&format!("global hotkeys registered: chat={chat}, scheduler={scheduler}, robot={robot}"));
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
    ROBOT_VISIBLE_IN_CAPTURE.store(load_robot_capture_setting(), AtomicOrdering::Relaxed);
    ROBOT_MUTED.store(load_robot_muted_setting(), AtomicOrdering::Relaxed);
    REMINDER_MUTED.store(load_reminder_muted_setting(), AtomicOrdering::Relaxed);
    if !acquire_single_instance() {
        bridge::log("ignored duplicate launch: an Aalu instance is already running");
        return Ok(());
    }

    let mut builder =
        EventLoopBuilder::<AppEvent>::with_user_event();

    let hotkey_proxy =
        Arc::new(Mutex::new(None::<EventLoopProxy<AppEvent>>));

    let hook_proxy = hotkey_proxy.clone();

    builder.with_msg_hook(move |raw| {
        let msg = unsafe { &*(raw as *const NativeMessage) };
        if msg.message == WM_HOTKEY {
            bridge::log(&format!("global WM_HOTKEY id={}", msg.wparam));
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

    // Re-assert the robot's TOPMOST flag periodically; Windows can push it behind
    // other topmost/fullscreen windows, leaving it alive but invisible.
    let keep_on_top_proxy = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let _ = keep_on_top_proxy.send_event(AppEvent::KeepOnTop);
    });

    // Lets the toolbar close itself when the user clicks anywhere outside it.
    let focus_check_proxy = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = focus_check_proxy.send_event(AppEvent::ToolbarFocusCheck);
    });

    // Mouse Button 4 (XBUTTON2) toggles the Personal Toolbar.
    let toolbar_mouse_proxy = proxy.clone();
    let _toolbar_mouse_thread = std::thread::spawn(move || {
        let mut was_down = false;
        loop {
            let down = unsafe { GetAsyncKeyState(VK_XBUTTON2) as u16 & 0x8000 != 0 };
            if down && !was_down { let _ = toolbar_mouse_proxy.send_event(AppEvent::ToggleToolbar); }
            was_down = down;
            std::thread::sleep(std::time::Duration::from_millis(35));
        }
    });

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
            hide_on_capture: || !ROBOT_VISIBLE_IN_CAPTURE.load(AtomicOrdering::Relaxed),
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
        .with_visible(false)
        .build(&event_loop)
        .expect("create robot window");

    apply_robot_capture_affinity(&robot_window);

    let robot_webview = {
        let mut created = None;
        let mut last_error = None;
        for attempt in 1..=3 {
            let robot_proxy = proxy.clone();
            match WebViewBuilder::new()
                .with_autoplay(true)
                .with_initialization_script(&format!("window.__robotMuted = {}; window.__reminderMuted = {};", ROBOT_MUTED.load(AtomicOrdering::Relaxed), REMINDER_MUTED.load(AtomicOrdering::Relaxed)))
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
    let mut tablet_status_window: Option<Window> = None;
    let mut tablet_status_webview: Option<WebView> = None;

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
    let mut bridge_tablet_mode = false;
    let mut bridge_connected = false;
    let mut bridge_status = "Waiting for tablet".to_string();
    let mut tablet_status_generation = 0u64;
    let mut scheduler_window: Option<Window> = None;
    let mut scheduler_webview: Option<WebView> = None;
    let mut toolbar_window: Option<Window> = None;
    let mut toolbar_webview: Option<WebView> = None;
    let mut toolbar_was_visible = false;
    let mut toolbar_had_focus = false;
    let mut toolbar_picking = false;
    let mut tablet_status_was_visible = false;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(AppEvent::ToggleToolbar) => {
                // Open only when the cursor is on the robot; if already open, the button closes it.
                let toolbar_open = toolbar_window.as_ref().map(Window::is_visible).unwrap_or(false);
                if toolbar_open || cursor_over_window(&robot_window) {
                    toggle_toolbar(target, &mut toolbar_window, &mut toolbar_webview, &proxy);
                    toolbar_had_focus = false;
                }
            }

            Event::UserEvent(AppEvent::ToolbarPicked(path)) => {
                toolbar_picking = false;
                if let Some(view) = toolbar_webview.as_ref() {
                    let js = serde_json::to_string(&path).unwrap_or_else(|_| "\"\"".into());
                    let _ = view.evaluate_script(&format!("window.__picked && window.__picked({js})"));
                }
                if let Some(window) = toolbar_window.as_ref() {
                    if window.is_visible() { window.set_focus(); }
                }
            }

            Event::UserEvent(AppEvent::ToolbarFocusCheck) => {
                if let Some(window) = toolbar_window.as_ref() {
                    if window.is_visible() {
                        let focused = unsafe { GetForegroundWindow() } == window.hwnd() as *mut c_void;
                        if focused {
                            toolbar_had_focus = true;
                        } else if toolbar_had_focus && !toolbar_picking {
                            // It had focus and lost it: the user clicked somewhere else.
                            window.set_visible(false);
                            toolbar_had_focus = false;
                        }
                    } else {
                        toolbar_had_focus = false;
                    }
                }
            }

            Event::UserEvent(AppEvent::ToolbarMessage(raw)) => {
                if raw == "close" {
                    if let Some(window) = toolbar_window.as_ref() { window.set_visible(false); }
                } else if matches!(raw.as_str(), "toggle-robot" | "open-chat" | "open-scheduler") {
                    if let Some(window) = toolbar_window.as_ref() { window.set_visible(false); }
                    let _ = proxy.send_event(match raw.as_str() {
                        "toggle-robot" => AppEvent::ToggleRobotHotkey,
                        "open-chat" => AppEvent::ToggleChatHotkey,
                        _ => AppEvent::ToggleSchedulerHotkey,
                    });
                } else if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(visible) = msg.get("robot_visible_in_capture").and_then(|v| v.as_bool()) {
                        ROBOT_VISIBLE_IN_CAPTURE.store(visible, AtomicOrdering::Relaxed);
                        save_robot_capture_setting(visible);
                        apply_robot_capture_affinity(&robot_window);
                    }
                    if let Some(muted) = msg.get("robot_muted").and_then(|v| v.as_bool()) {
                        ROBOT_MUTED.store(muted, AtomicOrdering::Relaxed);
                        save_robot_muted_setting(muted);
                        let _ = robot_webview.evaluate_script(&format!("window.__setMuted && window.__setMuted({muted})"));
                    }
                    if let Some(muted) = msg.get("reminder_muted").and_then(|v| v.as_bool()) {
                        REMINDER_MUTED.store(muted, AtomicOrdering::Relaxed);
                        save_reminder_muted_setting(muted);
                        let _ = robot_webview.evaluate_script(&format!("window.__setReminderMuted && window.__setReminderMuted({muted})"));
                    }
                    let target = msg.get("target").and_then(|v| v.as_str()).unwrap_or("");
                    match msg.get("t").and_then(|v| v.as_str()) {
                        Some("open-url") if target.to_ascii_lowercase().starts_with("https://") || target.to_ascii_lowercase().starts_with("http://") => {
                            notify_toolbar_action(&toolbar_webview, shell_open(target), "Opened website", true);
                        }
                        Some("open-url") => notify_toolbar_action(&toolbar_webview, Err("Only http:// and https:// website addresses are supported".into()), "", false),
                        Some("launch-app") if !target.trim().is_empty() => {
                            notify_toolbar_action(&toolbar_webview, shell_open(target), "Opened item", true);
                        }
                        Some("launch-app") => notify_toolbar_action(&toolbar_webview, Err("Choose an application, shortcut or folder first".into()), "", false),
                        Some("copy-text") => notify_toolbar_action(&toolbar_webview, set_clipboard_text(target), "Copied to clipboard", false),
                        Some("pick") => {
                            toolbar_picking = true;
                            pick_path(msg.get("mode").and_then(|v| v.as_str()).unwrap_or("file"), proxy.clone());
                        }
                        Some("set-setting") => {
                            if let Some(val) = msg.get("robot_visible_in_capture").and_then(|v| v.as_bool()) {
                                ROBOT_VISIBLE_IN_CAPTURE.store(val, AtomicOrdering::Relaxed);
                                save_robot_capture_setting(val);
                                apply_robot_capture_affinity(&robot_window);
                            }
                        }
                        _ => {}
                    }
                }
            }

            Event::UserEvent(AppEvent::KeepOnTop) => {
                if robot_window.is_visible() {
                    robot_window.set_always_on_top(false);
                    robot_window.set_always_on_top(true);
                }
                apply_robot_capture_affinity(&robot_window);
            }

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
                if message == "bridge-ready" {
                    let _ = robot_webview.evaluate_script(&format!("window.__setTabletMode && window.__setTabletMode({bridge_tablet_mode})"));
                } else if message == "open-chat" {
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
                    Some("check") => {
                        // Checklist tick: updates only the done-state and saves it right away,
                        // without touching any unsaved template edits in the scheduler window.
                        let date = ipc["date"].as_str().unwrap_or("");
                        let block_id = ipc["id"].as_str().unwrap_or("");
                        let done = ipc["done"].as_bool().unwrap_or(false);
                        let result: Result<(), String> = {
                            let mut guard = shared_sched.0.lock().unwrap();
                            if scheduler::set_block_done(&mut guard, date, block_id, done) {
                                scheduler::save_schedule(&mut guard)
                            } else {
                                Err("Invalid checklist entry".into())
                            }
                        };
                        if let Some(view) = scheduler_webview.as_ref() {
                            match result {
                                Ok(()) => { let _ = view.evaluate_script("window.__sched.checkResult(true, null)"); }
                                Err(err) => {
                                    let err_js = serde_json::to_string(&err).unwrap_or_else(|_| "\"Save error\"".into());
                                    let _ = view.evaluate_script(&format!("window.__sched.checkResult(false, {err_js})"));
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
                bridge::log(&format!("ui got bridge event: {event:?}"));
                match event {
                    bridge::Event::Mode(on) => {
                        bridge_tablet_mode = on;
                        let _ = robot_webview.evaluate_script(&format!("window.__setTabletMode && window.__setTabletMode({on})"));
                        tablet_status_generation = tablet_status_generation.wrapping_add(1);
                        show_tablet_status(
                            target, &robot_window, &proxy,
                            &mut tablet_status_window, &mut tablet_status_webview,
                            bridge_connected, bridge_tablet_mode, &bridge_status,
                        );
                        if !on {
                            schedule_tablet_status_hide(&proxy, &mut tablet_status_generation, 3500);
                        }
                    }
                    bridge::Event::Refused(why) => {
                        bridge_status = why;
                        show_tablet_status(
                            target, &robot_window, &proxy,
                            &mut tablet_status_window, &mut tablet_status_webview,
                            bridge_connected, bridge_tablet_mode, &bridge_status,
                        );
                        schedule_tablet_status_hide(&proxy, &mut tablet_status_generation, 5000);
                    }
                    bridge::Event::Link { connected, message } => {
                        let was_connected = bridge_connected;
                        bridge_connected = connected;
                        bridge_status = if connected { "USB tablet connected".into() } else { message };
                        if tablet_status_window.is_some() && tablet_status_window.as_ref().is_some_and(Window::is_visible) {
                            update_tablet_status(&mut tablet_status_webview, connected, bridge_tablet_mode, &bridge_status);
                        } else if was_connected && !connected {
                            show_tablet_status(
                                target, &robot_window, &proxy,
                                &mut tablet_status_window, &mut tablet_status_webview,
                                bridge_connected, bridge_tablet_mode, &bridge_status,
                            );
                            schedule_tablet_status_hide(&proxy, &mut tablet_status_generation, 5000);
                        }
                    }
                }
            }

            Event::UserEvent(AppEvent::TabletStatusMessage(raw)) => {
                let ipc: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
                if ipc["t"] == "ready" {
                    update_tablet_status(&mut tablet_status_webview, bridge_connected, bridge_tablet_mode, &bridge_status);
                } else if ipc["t"] == "close" {
                    tablet_status_generation = tablet_status_generation.wrapping_add(1);
                    if let Some(window) = tablet_status_window.as_ref() { window.set_visible(false); }
                }
            }

            Event::UserEvent(AppEvent::HideTabletStatus(generation)) => {
                if generation == tablet_status_generation && !bridge_tablet_mode {
                    if let Some(window) = tablet_status_window.as_ref() { window.set_visible(false); }
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
                        if !show {
                            apply_robot_capture_affinity(&robot_window);
                        }
                        robot_window.set_visible(show);
                        let _ = robot_webview.evaluate_script(&format!("window.__setAppVisible({show})"));

                        if let Some(window) = chat_window.as_ref() {
                            window.set_visible(show);
                        }
                        if let Some(window) = scheduler_window.as_ref() {
                            window.set_visible(show);
                        }
                        if !show {
                            toolbar_was_visible = toolbar_window.as_ref().map(Window::is_visible).unwrap_or(false);
                            if let Some(window) = toolbar_window.as_ref() { window.set_visible(false); }
                            tablet_status_was_visible = tablet_status_window.as_ref().map(Window::is_visible).unwrap_or(false);
                            if let Some(window) = tablet_status_window.as_ref() { window.set_visible(false); }
                        } else {
                            if toolbar_was_visible {
                                if let Some(window) = toolbar_window.as_ref() { window.set_visible(true); }
                                toolbar_was_visible = false;
                            }
                            if tablet_status_was_visible {
                                if let Some(window) = tablet_status_window.as_ref() { window.set_visible(true); }
                                tablet_status_was_visible = false;
                            }
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
                if let Some(window) = tablet_status_window.as_ref() {
                    if window.is_visible() {
                        window.set_outer_position(aligned_window_position(target, &robot_window, TABLET_STATUS_WIDTH, TABLET_STATUS_HEIGHT, is_reminder_active));
                    }
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
                } else if toolbar_window.as_ref().map(Window::id) == Some(window_id) {
                    if let Some(window) = toolbar_window.as_ref() { window.set_visible(false); }
                } else if tablet_status_window.as_ref().map(Window::id) == Some(window_id) {
                    if let Some(window) = tablet_status_window.as_ref() { window.set_visible(false); }
                }
            }

            _ => {}
        }
    });
}

/// Native file/folder chooser for the Personal Toolbar's Add dialog (PowerShell, no extra crates).
fn pick_path(mode: &str, proxy: EventLoopProxy<AppEvent>) {
    let body = if mode == "folder" {
        "$d=New-Object System.Windows.Forms.FolderBrowserDialog; if($d.ShowDialog($f) -eq 'OK'){[Console]::Out.Write($d.SelectedPath)}"
    } else {
        "$d=New-Object System.Windows.Forms.OpenFileDialog; $d.DereferenceLinks=$false; if($d.ShowDialog($f) -eq 'OK'){[Console]::Out.Write($d.FileName)}"
    };
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Add-Type -AssemblyName System.Windows.Forms; $f=New-Object System.Windows.Forms.Form -Property @{{TopMost=$true}}; {body}"
    );
    std::thread::spawn(move || {
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-STA", "-WindowStyle", "Hidden", "-Command", &script])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output();
        let path = match output {
            Ok(result) if result.status.success() => String::from_utf8_lossy(&result.stdout).trim().to_string(),
            Ok(result) => {
                let detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
                if detail.is_empty() { format!("__AALU_ERROR__File picker exited with status {}", result.status) }
                else { format!("__AALU_ERROR__File picker failed: {}", detail.chars().take(180).collect::<String>()) }
            }
            Err(error) => format!("__AALU_ERROR__Could not start Windows file picker: {error}"),
        };
        let _ = proxy.send_event(AppEvent::ToolbarPicked(path));
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

fn show_tablet_status(
    target: &EventLoopWindowTarget<AppEvent>,
    robot_window: &Window,
    proxy: &EventLoopProxy<AppEvent>,
    window_slot: &mut Option<Window>,
    webview_slot: &mut Option<WebView>,
    connected: bool,
    tablet_mode: bool,
    message: &str,
) {
    if window_slot.is_none() {
        let window = match WindowBuilder::new()
            .with_title("Aalu Tablet Status")
            .with_inner_size(LogicalSize::new(TABLET_STATUS_WIDTH, TABLET_STATUS_HEIGHT))
            .with_position(aligned_window_position(target, robot_window, TABLET_STATUS_WIDTH, TABLET_STATUS_HEIGHT, false))
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_skip_taskbar(true)
                .with_visible(false)
            .build(target)
        {
            Ok(window) => window,
            Err(error) => { eprintln!("Could not create tablet status window: {error}"); bridge::log(&format!("status window FAILED: {error}")); return; }
        };
        let status_proxy = proxy.clone();
        let webview = match WebViewBuilder::new()
            .with_html(TABLET_STATUS_HTML)
            .with_ipc_handler(move |request| {
                let _ = status_proxy.send_event(AppEvent::TabletStatusMessage(request.body().clone()));
            })
            .build(&window)
        {
            Ok(view) => view,
            Err(error) => { eprintln!("Could not create tablet status view: {error}"); bridge::log(&format!("status webview FAILED: {error}")); return; }
        };
        *window_slot = Some(window);
        *webview_slot = Some(webview);
    }

    if let Some(window) = window_slot.as_ref() {
        window.set_outer_position(aligned_window_position(target, robot_window, TABLET_STATUS_WIDTH, TABLET_STATUS_HEIGHT, false));
        window.set_visible(true);
    }
    update_tablet_status(webview_slot, connected, tablet_mode, message);
}

fn update_tablet_status(webview_slot: &mut Option<WebView>, connected: bool, tablet_mode: bool, message: &str) {
    let message = serde_json::to_string(message).unwrap_or_else(|_| "\"Tablet status unavailable\"".into());
    if let Some(view) = webview_slot.as_ref() {
        let _ = view.evaluate_script(&format!("window.__tabletStatus && window.__tabletStatus({connected}, {tablet_mode}, {message})"));
    }
}

fn schedule_tablet_status_hide(proxy: &EventLoopProxy<AppEvent>, generation: &mut u64, delay_ms: u64) {
    *generation = generation.wrapping_add(1);
    let generation = *generation;
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let _ = proxy.send_event(AppEvent::HideTabletStatus(generation));
    });
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


fn toggle_toolbar(
    target: &EventLoopWindowTarget<AppEvent>,
    window_slot: &mut Option<Window>,
    webview_slot: &mut Option<WebView>,
    proxy: &EventLoopProxy<AppEvent>,
) {
    if let Some(window) = window_slot.as_ref() {
        let visible = window.is_visible();
        if !visible {
            if let Some(view) = webview_slot.as_ref() {
                let show = ROBOT_VISIBLE_IN_CAPTURE.load(AtomicOrdering::Relaxed);
                let muted = ROBOT_MUTED.load(AtomicOrdering::Relaxed);
                let reminder_muted = REMINDER_MUTED.load(AtomicOrdering::Relaxed);
                let _ = view.evaluate_script(&format!("window.__setSettings && window.__setSettings({{showRobotInCapture: {show}, robotMuted: {muted}, reminderMuted: {reminder_muted}}}); window.__reset && window.__reset();"));
            }
            center_toolbar(target, window);
        }
        window.set_visible(!visible);
        if !visible { window.set_focus(); }
        return;
    }
    let window = match WindowBuilder::new()
        .with_title("Aalu Personal Toolbar")
        .with_window_icon(app_icon::window_icon())
        .with_inner_size(LogicalSize::new(TOOLBAR_WIDTH, TOOLBAR_HEIGHT))
        .with_always_on_top(true)
        .with_decorations(false)
        .with_undecorated_shadow(false)
        .with_transparent(true)
        .with_skip_taskbar(true)
        .with_resizable(false)
        .build(target)
    {
        Ok(window) => window,
        Err(error) => { eprintln!("Could not create Personal Toolbar window: {error}"); return; }
    };
    center_toolbar(target, &window);
    let toolbar_proxy = proxy.clone();
    match WebViewBuilder::new()
        // A named secure origin is required for WebView2 localStorage persistence.
        .with_https_scheme(true)
        .with_initialization_script(&format!("window.__nativeSettings = {{ robotMuted: {}, reminderMuted: {} }};", ROBOT_MUTED.load(AtomicOrdering::Relaxed), REMINDER_MUTED.load(AtomicOrdering::Relaxed)))
        .with_custom_protocol("aalu".into(), |_webview_id, _request| {
            wry::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Cow::Borrowed(PERSONAL_TOOLBAR_HTML.as_bytes()))
                .expect("valid embedded toolbar response")
        })
        .with_url("aalu://toolbar/index.html")
        .with_transparent(true)
        .with_ipc_handler(move |request| { let _ = toolbar_proxy.send_event(AppEvent::ToolbarMessage(request.body().clone())); })
        .build(&window)
    {
        Ok(view) => {
            *window_slot = Some(window);
            *webview_slot = Some(view);
        }
        Err(error) => eprintln!("Could not create Personal Toolbar WebView: {error}"),
    }
}

// Centre the toolbar on the primary monitor (physical pixels, DPI-safe).
fn center_toolbar(target: &EventLoopWindowTarget<AppEvent>, window: &Window) {
    if let Some(monitor) = target.primary_monitor() {
        let scale = monitor.scale_factor();
        let pos = monitor.position();
        let size = monitor.size();
        let w = (TOOLBAR_WIDTH * scale).round() as i32;
        let h = (TOOLBAR_HEIGHT * scale).round() as i32;
        window.set_outer_position(PhysicalPosition::new(
            pos.x + (size.width as i32 - w) / 2,
            pos.y + (size.height as i32 - h) / 2,
        ));
    }
}
