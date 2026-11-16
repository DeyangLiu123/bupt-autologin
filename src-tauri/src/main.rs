// 发布版在 Windows 上不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod icon;
mod monitor;
mod portal;
mod tray;

use config::Settings;
use monitor::{Monitor, Snapshot};
use serde::Serialize;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// 主窗口按需创建、关闭即销毁，后台只保留托盘和监控线程。
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("北邮校园网自动登录")
        .inner_size(380.0, 700.0)
        .resizable(false)
        .maximizable(false)
        .center()
        .focused(true)
        .build();
}

#[derive(Serialize)]
struct View {
    snapshot: Snapshot,
    settings: Settings,
    has_password: bool,
    autostart: bool,
    platform: &'static str,
    version: &'static str,
}

#[tauri::command]
fn get_state(app: AppHandle, monitor: State<Monitor>) -> View {
    View {
        snapshot: monitor.snapshot(),
        settings: monitor.settings(),
        has_password: monitor.has_password(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        platform: std::env::consts::OS,
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
fn save_account(monitor: State<Monitor>, username: String, password: String) -> Result<(), String> {
    let username = username.trim().to_string();
    if username.is_empty() || password.is_empty() {
        return Err("学号和密码都不能为空".into());
    }
    monitor.save_account(username, password)
}

#[tauri::command]
fn clear_account(monitor: State<Monitor>) -> Result<(), String> {
    monitor.clear_account()
}

#[tauri::command]
fn set_auto_login(app: AppHandle, monitor: State<Monitor>, enabled: bool) -> Result<(), String> {
    monitor.update_settings(|s| s.auto_login = enabled)?;
    tray::sync_checks(&app, enabled, app.autolaunch().is_enabled().unwrap_or(false));
    Ok(())
}

#[tauri::command]
fn set_notify(monitor: State<Monitor>, enabled: bool) -> Result<(), String> {
    monitor.update_settings(|s| s.notify = enabled)
}

#[tauri::command]
fn set_gateway(monitor: State<Monitor>, gateway: String) -> Result<String, String> {
    let mut gateway = gateway.trim().trim_end_matches('/').to_string();
    if gateway.is_empty() {
        gateway = config::DEFAULT_GATEWAY.into();
    }
    if !gateway.starts_with("http://") && !gateway.starts_with("https://") {
        gateway = format!("http://{gateway}");
    }
    let saved = gateway.clone();
    monitor.update_settings(|s| s.gateway = gateway)?;
    Ok(saved)
}

#[tauri::command]
fn set_autostart(app: AppHandle, monitor: State<Monitor>, enabled: bool) -> Result<(), String> {
    tray::set_autostart(&app, enabled)?;
    tray::sync_checks(&app, monitor.settings().auto_login, enabled);
    Ok(())
}

#[tauri::command]
fn check_now(monitor: State<Monitor>) {
    monitor.request(true);
}

/// 在系统文件管理器里显示日志文件。
#[tauri::command]
fn open_log(monitor: State<Monitor>) -> Result<(), String> {
    let path = monitor.log_path();
    let dir = path.parent().unwrap_or(path);
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(dir).spawn();
    let _ = dir;
    result.map(|_| ()).map_err(|e| format!("无法打开日志目录：{e}"))
}

fn main() {
    tauri::Builder::default()
        // 重复启动时只把已运行实例的窗口调出来
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--hidden"])))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_account,
            clear_account,
            set_auto_login,
            set_notify,
            set_gateway,
            set_autostart,
            check_now,
            open_log,
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let dir = app.path().app_config_dir()?;
            let path = dir.join("settings.json");
            let first_run = !path.exists();
            let settings = Settings::load(&path).unwrap_or_default();
            if first_run {
                let _ = settings.save(&path);
                let _ = tray::set_autostart(app.handle(), true);
            }

            let log_path = app.path().app_log_dir()?.join("bupt-autologin.log");
            let monitor = Monitor::new(app.handle().clone(), dir, log_path, settings.clone());
            app.manage(monitor.clone());
            if let Err(e) = tray::setup(app, settings.auto_login) {
                // 部分 Linux 桌面没有托盘支持，此时退化为只有窗口
                eprintln!("托盘初始化失败：{e}");
            }
            monitor.start();

            let hidden = std::env::args().any(|a| a == "--hidden");
            if !hidden || settings.username.is_empty() {
                show_window(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动失败")
        .run(|_app, event| {
            // 关掉窗口不退出，继续在托盘运行；只有菜单里的「退出」会带退出码
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
}
