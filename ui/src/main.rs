//! Switchboard desktop app: tray icon + window hosting the desk UI in the
//! system web view (WebView2 / WebKitGTK). Talks to the background service
//! over its local API; never touches input itself, so a UI crash cannot affect
//! forwarding. The window (and web view) is destroyed when closed.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod daemon;
mod icon;

use std::time::Duration;

use serde_json::{json, Value};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use wry::{WebView, WebViewBuilder};

const HTML: &str = include_str!("../web/index.html");
const CSS: &str = include_str!("../web/app.css");
const JS: &str = include_str!("../web/app.js");
const THREE: &str = include_str!("../web/vendor/three.min.js");
const DESK3D: &str = include_str!("../web/desk3d.js");
const DEMO: &str = include_str!("../web/demo.js");

#[derive(Debug)]
enum UserEvent {
    Reply(u64, Value),
    Status(Option<Value>),
    Show,
    Menu(MenuEvent),
    Tray(TrayIconEvent),
}

struct Ui {
    window: Window,
    webview: WebView,
}

fn page() -> String {
    HTML.replace("/*CSS*/", CSS).replace("/*THREE*/", THREE).replace("/*DESK3D*/", DESK3D).replace("/*DEMO*/", DEMO).replace("/*JS*/", JS)
}

fn main() {
    let background = std::env::args().any(|a| a == "--background");
    let demo = std::env::args().any(|a| a == "--demo");
    if demo {
        std::env::set_var("SWITCHBOARD_DEMO", "1");
    }
    // Single instance: a second launch just asks the first to show its window.
    let Some(guard) = daemon::single_instance() else {
        daemon::poke_existing();
        return;
    };
    if !demo {
        daemon::ensure_running();
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Tray(e));
        }));
    }
    daemon::listen_for_show(guard, proxy.clone(), || UserEvent::Show);
    // Status poller drives the tray.
    {
        let p = proxy.clone();
        std::thread::spawn(move || loop {
            let s = daemon::request(json!({"cmd": "status"})).ok();
            if p.send_event(UserEvent::Status(s)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1000));
        });
    }

    let menu = Menu::new();
    let m_status = MenuItem::new("Starting…", false, None);
    let m_open = MenuItem::new("Open Switchboard", true, None);
    let m_switch = MenuItem::new("Send pointer to…", false, None);
    let m_pause = MenuItem::new("Pause", true, None);
    let m_quit = MenuItem::new("Quit Switchboard", true, None);
    let _ = menu.append_items(&[&m_status, &PredefinedMenuItem::separator(), &m_open, &m_switch, &m_pause, &PredefinedMenuItem::separator(), &m_quit]);

    let mut tray: Option<TrayIcon> = None;
    let mut ui: Option<Ui> = None;
    let mut last_status: Option<Value> = None;
    let mut want_show = !background;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new().with_menu(Box::new(menu.clone())).with_tooltip("Switchboard").with_icon(icon::tray(icon::State::Idle)).build().ok();
                if want_show {
                    ui = open_window(target, &proxy);
                    want_show = false;
                }
            }
            Event::UserEvent(UserEvent::Show) => {
                if let Some(u) = &ui {
                    u.window.set_visible(true);
                    u.window.set_focus();
                } else {
                    ui = open_window(target, &proxy);
                }
            }
            Event::UserEvent(UserEvent::Reply(id, v)) => {
                if let Some(u) = &ui {
                    let _ = u.webview.evaluate_script(&format!("window.sb && window.sb._reply({id}, {v})"));
                }
            }
            Event::UserEvent(UserEvent::Status(s)) => {
                let state = icon::state_of(s.as_ref());
                if let Some(t) = &tray {
                    let _ = t.set_icon(Some(icon::tray(state)));
                    let _ = t.set_tooltip(Some(icon::tooltip(s.as_ref())));
                }
                m_status.set_text(icon::tooltip(s.as_ref()));
                match s.as_ref() {
                    Some(v) => {
                        let disabled = v["disabled"] == true;
                        m_pause.set_text(if disabled { "Resume" } else { "Pause" });
                        let mode = v["mode"].as_str().unwrap_or("local");
                        let peer = v["peers"].as_array().and_then(|a| a.iter().find(|p| p["trusted"] == true)).and_then(|p| p["name"].as_str()).map(str::to_string);
                        if mode != "local" {
                            m_switch.set_text("Bring pointer back here");
                            m_switch.set_enabled(true);
                        } else if let Some(p) = peer {
                            m_switch.set_text(format!("Send pointer to {p}"));
                            m_switch.set_enabled(true);
                        } else {
                            m_switch.set_text("Send pointer to…");
                            m_switch.set_enabled(false);
                        }
                    }
                    None => {
                        m_switch.set_enabled(false);
                    }
                }
                last_status = s;
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == *m_open.id() {
                    let _ = proxy.send_event(UserEvent::Show);
                } else if e.id == *m_quit.id() {
                    let _ = daemon::request(json!({"cmd": "quit"}));
                    *control_flow = ControlFlow::Exit;
                } else if e.id == *m_pause.id() {
                    let disabled = last_status.as_ref().is_some_and(|v| v["disabled"] == true);
                    let _ = daemon::request(json!({"cmd": if disabled { "enable" } else { "disable" }}));
                } else if e.id == *m_switch.id() {
                    let s = last_status.clone().unwrap_or(Value::Null);
                    let to = if s["mode"].as_str().unwrap_or("local") != "local" {
                        "local".to_string()
                    } else {
                        s["peers"].as_array().and_then(|a| a.iter().find(|p| p["trusted"] == true)).and_then(|p| p["name"].as_str()).unwrap_or("local").to_string()
                    };
                    std::thread::spawn(move || {
                        let _ = daemon::request(json!({"cmd": "switch", "to": to}));
                    });
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::DoubleClick { .. })) => {
                let _ = proxy.send_event(UserEvent::Show);
            }
            Event::UserEvent(UserEvent::Tray(_)) => {}
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Destroy the window + web view; the tray keeps running.
                ui = None;
            }
            _ => {}
        }
    });
}

fn open_window(target: &tao::event_loop::EventLoopWindowTarget<UserEvent>, proxy: &EventLoopProxy<UserEvent>) -> Option<Ui> {
    let window = WindowBuilder::new()
        .with_title("Switchboard")
        .with_inner_size(tao::dpi::LogicalSize::new(1120.0, 720.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(860.0, 560.0))
        .with_window_icon(Some(icon::window_icon()))
        .build(target)
        .ok()?;
    let p = proxy.clone();
    let builder = WebViewBuilder::new()
        .with_html(page())
        .with_initialization_script(&format!(
            "window.__SB_VIEW = {:?}; window.__SB_OPEN = {:?}; window.__SB_DEMO = {};",
            std::env::var("SWITCHBOARD_VIEW").unwrap_or_default(),
            std::env::var("SWITCHBOARD_OPEN").unwrap_or_default(),
            std::env::var("SWITCHBOARD_DEMO").is_ok()
        ))
        .with_devtools(std::env::var("SWITCHBOARD_DEVTOOLS").is_ok())
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let p = p.clone();
            let body = req.body().clone();
            std::thread::spawn(move || {
                let Ok(v) = serde_json::from_str::<Value>(&body) else { return };
                let id = v["id"].as_u64().unwrap_or(0);
                let r = handle(v["req"].clone());
                let _ = p.send_event(UserEvent::Reply(id, r));
            });
        });
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window).ok()?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox()?).ok()?
    };
    Some(Ui { window, webview })
}

/// Requests from the page: app-local commands, everything else to the service.
fn handle(req: Value) -> Value {
    match req["cmd"].as_str().unwrap_or("") {
        "app.autostart_get" => json!({"ok": true, "enabled": daemon::autostart_enabled()}),
        "app.autostart_set" => match daemon::set_autostart(req["enabled"].as_bool().unwrap_or(false)) {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        "app.open_downloads" => {
            daemon::open_path(&daemon::downloads_dir());
            json!({"ok": true})
        }
        "app.platform" => json!({"ok": true, "os": std::env::consts::OS}),
        "app.start_service" => {
            daemon::ensure_running();
            json!({"ok": true})
        }
        _ => daemon::request(req).unwrap_or_else(|e| json!({"ok": false, "error": e, "offline": true})),
    }
}
