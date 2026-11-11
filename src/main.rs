#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod backend;
mod chat;
mod keys;
mod screen;

use std::{ffi::c_void, sync::{Arc, Mutex}};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{EventLoopBuilderExtWindows, WindowBuilderExtWindows},
    window::{Window, WindowBuilder},
};
use wry::{WebView, WebViewBuilder};

const ROBOT_HTML: &str = include_str!("../assets/robot.html");
const CHAT_HTML: &str = include_str!("../assets/chat.html");

enum AppEvent {
    GlobalHotkey,
    RobotMessage(String),
    ChatMessage(String),
    BackendOut(backend::Out),
}

#[repr(C)]
struct NativePoint { x: i32, y: i32 }
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
    fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
}

const ROBOT_MENU_SCRIPT: &str = r#"
document.addEventListener('contextmenu', (event) => {
  event.preventDefault();
  document.getElementById('__ai_companion_quit_menu')?.remove();
  const menu = document.createElement('button'); menu.id = '__ai_companion_quit_menu';
  menu.type = 'button'; menu.textContent = 'Quit';
  Object.assign(menu.style, {position:'fixed',left:`${event.clientX}px`,top:`${event.clientY}px`,zIndex:'2147483647',padding:'8px 16px',border:'1px solid #ccd2df',borderRadius:'8px',background:'#fff',color:'#27334c',font:'14px Segoe UI,sans-serif',boxShadow:'0 4px 14px #0003',cursor:'pointer'});
  menu.addEventListener('click', () => window.ipc.postMessage('quit')); document.body.appendChild(menu);
  const dismiss = (pointer) => { if (!menu.contains(pointer.target)) { menu.remove(); document.removeEventListener('pointerdown', dismiss, true); } };
  setTimeout(() => document.addEventListener('pointerdown', dismiss, true), 0);
}, true);
"#;

fn register_global_hotkey() -> bool {
    let hwnd = std::ptr::null_mut();
    if unsafe { RegisterHotKey(hwnd, HOTKEY_ID, MOD_ALT | MOD_NOREPEAT, VK_SPACE) } != 0 { return true; }
    eprintln!("Could not register Alt+Space; trying Ctrl+Alt+Space instead.");
    if unsafe { RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_SPACE) } == 0 {
        eprintln!("Could not register Ctrl+Alt+Space either; the app will run without a global hotkey.");
        false
    } else { true }
}
fn unregister_global_hotkey() { unsafe { UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID); } }
fn screen_text() -> Result<String, String> { let shot = screen::capture_screen()?; screen::ocr(&shot) }
fn screen_jpeg() -> Result<String, String> { let shot = screen::capture_screen()?; Ok(ai::base64(&screen::jpeg(&shot)?)) }

fn main() -> wry::Result<()> {
    let mut builder = EventLoopBuilder::<AppEvent>::with_user_event();
    let hotkey_proxy = Arc::new(Mutex::new(None::<EventLoopProxy<AppEvent>>));
    let hook_proxy = hotkey_proxy.clone();
    builder.with_msg_hook(move |raw| {
        let msg = unsafe { &*(raw as *const NativeMessage) };
        if msg.message == WM_HOTKEY && msg.wparam == HOTKEY_ID as usize {
            if let Some(proxy) = hook_proxy.lock().unwrap().as_ref().cloned() { let _ = proxy.send_event(AppEvent::GlobalHotkey); }
            true
        } else { false }
    });
    let event_loop = builder.build();
    let proxy = event_loop.create_proxy();
    *hotkey_proxy.lock().unwrap() = Some(proxy.clone());
    let hotkey_registered = register_global_hotkey();
    let hotkey_name = if hotkey_registered { "Alt+Space" } else { "Ctrl+Alt+Space" };
    let emit_proxy = proxy.clone();
    let emit: backend::Emit = Arc::new(move |out| { let _ = emit_proxy.send_event(AppEvent::BackendOut(out)); });
    let backend = Arc::new(backend::Backend::new(
        ai::load_config(),
        backend::Deps { key_get: keys::get, key_set: keys::set, screen_text, screen_jpeg },
        emit,
        hotkey_name.to_string(),
    ));

    let robot_window = WindowBuilder::new().with_title("AI Companion")
        .with_inner_size(LogicalSize::new(120.0, 140.0)).with_position(LogicalPosition::new(40.0, 120.0))
        .with_decorations(false).with_transparent(true).with_always_on_top(true).with_skip_taskbar(true)
        .build(&event_loop).expect("create robot window");
    let robot_proxy = proxy.clone();
    let _robot_webview = WebViewBuilder::new(&robot_window).with_html(ROBOT_HTML)
        .with_initialization_script(ROBOT_MENU_SCRIPT).with_transparent(true)
        .with_ipc_handler(move |request| { let _ = robot_proxy.send_event(AppEvent::RobotMessage(request.body().clone())); })
        .build()?;

    let mut chat_window: Option<Window> = None;
    let mut chat_webview: Option<WebView> = None;
    let root_id = robot_window.id();
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(AppEvent::GlobalHotkey) => {
                robot_window.set_visible(true);
                open_chat(target, &proxy, &mut chat_window, &mut chat_webview);
            }
            Event::UserEvent(AppEvent::RobotMessage(message)) => {
                if message == "open-chat" { open_chat(target, &proxy, &mut chat_window, &mut chat_webview); }
                else if message == "quit" { unregister_global_hotkey(); *control_flow = ControlFlow::Exit; }
                else if let Some(delta) = message.strip_prefix("drag:") {
                    if let Some((dx, dy)) = delta.split_once(',').and_then(|(x,y)| Some((x.parse::<f64>().ok()?,y.parse::<f64>().ok()?))) {
                        if let Ok(pos) = robot_window.outer_position() { let scale=robot_window.scale_factor(); robot_window.set_outer_position(LogicalPosition::new(pos.x as f64/scale+dx,pos.y as f64/scale+dy)); }
                    }
                }
            }
            Event::UserEvent(AppEvent::ChatMessage(raw)) => backend.handle(&raw),
            Event::UserEvent(AppEvent::BackendOut(out)) => match out {
                backend::Out::Js(js) => if let Some(view) = chat_webview.as_ref() { let _ = view.evaluate_script(&js); },
                backend::Out::Overlays(show) => { robot_window.set_visible(show); if let Some(window)=chat_window.as_ref(){window.set_visible(show);} }
                backend::Out::CloseChat => { backend.reset(); chat_webview=None; chat_window=None; }
            },
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                if window_id == root_id { unregister_global_hotkey(); *control_flow=ControlFlow::Exit; }
                else if chat_window.as_ref().map(Window::id)==Some(window_id) { backend.reset(); chat_webview=None; chat_window=None; }
            }
            _ => {}
        }
    });
}

fn open_chat(target: &EventLoopWindowTarget<AppEvent>, proxy: &EventLoopProxy<AppEvent>, chat_window: &mut Option<Window>, chat_webview: &mut Option<WebView>) {
    if let Some(window)=chat_window.as_ref(){window.set_visible(true);window.set_focus();return;}
    let window=match WindowBuilder::new().with_title("AI Companion — Phase 0")
        .with_inner_size(LogicalSize::new(420.0,560.0)).with_position(LogicalPosition::new(180.0,140.0))
        .with_min_inner_size(LogicalSize::new(340.0,420.0)).with_always_on_top(true).build(target){
        Ok(w)=>w, Err(e)=>{eprintln!("Could not create chat window: {e}");return;}
    };
    let chat_proxy=proxy.clone();
    match WebViewBuilder::new(&window).with_html(CHAT_HTML)
        .with_ipc_handler(move |request| { let _=chat_proxy.send_event(AppEvent::ChatMessage(request.body().clone())); })
        .build(){
        Ok(view)=>{*chat_window=Some(window);*chat_webview=Some(view);}
        Err(e)=>eprintln!("Could not create chat WebView2: {e}"),
    }
}
