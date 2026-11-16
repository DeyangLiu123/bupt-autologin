use crate::{icon, monitor::{Monitor, Status}, show_window};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Emitter, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt;

const TRAY_ID: &str = "main";

pub struct TrayItems {
    status: MenuItem<Wry>,
    auto_login: CheckMenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
}

pub fn setup(app: &App, auto_login: bool) -> tauri::Result<()> {
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let status = MenuItem::with_id(app, "status", Status::Checking.label(), false, None::<&str>)?;
    let check = MenuItem::with_id(app, "check", "立即检测", true, None::<&str>)?;
    let auto = CheckMenuItem::with_id(app, "auto_login", "自动登录", true, auto_login, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", "开机启动", true, autostart_on, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "打开主界面…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&status, &check, &sep1, &auto, &autostart, &sep2, &open, &quit],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon::render(Status::Checking.color()))
        .tooltip("北邮校园网自动登录")
        .menu(&menu)
        // macOS 习惯单击弹菜单；Windows/Linux 左键打开主界面，右键弹菜单
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id.as_ref() {
            "check" => app.state::<Monitor>().request(true),
            "auto_login" => {
                let on = app.state::<TrayItems>().auto_login.is_checked().unwrap_or(true);
                let _ = app.state::<Monitor>().update_settings(|s| s.auto_login = on);
                let _ = app.emit("settings", ());
            }
            "autostart" => {
                let on = app.state::<TrayItems>().autostart.is_checked().unwrap_or(false);
                let _ = set_autostart(app, on);
                let _ = app.emit("settings", ());
            }
            "open" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if !cfg!(target_os = "macos") {
                    show_window(tray.app_handle());
                }
            }
        })
        .build(app)?;

    app.manage(TrayItems { status, auto_login: auto, autostart });
    Ok(())
}

pub fn refresh(app: &AppHandle, status: Status) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(icon::render(status.color())));
        let _ = tray.set_tooltip(Some(format!("北邮校园网：{}", status.label())));
    }
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.status.set_text(status.label());
    }
}

/// 界面里改了开关后同步托盘菜单的勾选状态。
pub fn sync_checks(app: &AppHandle, auto_login: bool, autostart: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.auto_login.set_checked(auto_login);
        let _ = items.autostart.set_checked(autostart);
    }
}

pub fn set_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    let result = if enabled { launcher.enable() } else { launcher.disable() };
    result.map_err(|e| format!("设置开机启动失败：{e}"))
}
