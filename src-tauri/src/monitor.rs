//! 后台监控线程。
//!
//! 为了省电省流量，平时每 3 秒只读一次本机网卡 IP（纯系统调用，无网络请求）；
//! IP 变化、睡眠唤醒或用户手动触发时立即检测，否则每 60 秒才发一次联网探测。

use crate::{
    config::{self, Settings},
    http,
    portal::{Portal, Probe},
    tray,
};
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

const TICK: Duration = Duration::from_secs(3);
const ONLINE_INTERVAL: Duration = Duration::from_secs(60);
const OFFLINE_INTERVAL: Duration = Duration::from_secs(30);
/// 网络刚变化时网关可能还没就绪，短间隔多试几次
const SETTLE_INTERVAL: Duration = Duration::from_secs(5);
const SETTLE_TRIES: u32 = 3;
const MAX_BACKOFF: u64 = 600;
const MAX_EVENTS: usize = 30;
const MAX_LOG_BYTES: u64 = 512 * 1024;
const BAD_PASSWORD_HINT: &str = "用户名或密码错误，已暂停自动登录，请更新密码";

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Checking,
    Online,
    OffCampus,
    NeedsLogin,
    LoggingIn,
    Failed,
    NoAccount,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Checking => "检测中…",
            Status::Online => "已联网",
            Status::OffCampus => "未连接校园网",
            Status::NeedsLogin => "校园网未认证",
            Status::LoggingIn => "正在登录…",
            Status::Failed => "登录失败",
            Status::NoAccount => "未设置账号",
        }
    }

    pub fn color(self) -> [u8; 3] {
        match self {
            Status::Online => [52, 199, 89],
            Status::Checking | Status::LoggingIn => [255, 159, 10],
            Status::Failed | Status::NoAccount => [255, 69, 58],
            Status::OffCampus | Status::NeedsLogin => [142, 142, 147],
        }
    }
}

#[derive(Clone, Serialize)]
pub struct Event {
    time: u64,
    level: &'static str,
    text: String,
}

#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub status: Status,
    pub detail: String,
    pub last_check: u64,
    pub logins: u32,
    pub events: Vec<Event>,
}

struct Inner {
    snap: Snapshot,
    settings: Settings,
    password: Option<String>,
    /// 网关明确拒绝了账号密码；在更新密码或手动检测前不再自动登录，避免账号被锁
    bad_password: bool,
    /// 待处理的检测请求；Some(true) 表示用户手动触发，忽略暂停和退避
    request: Option<bool>,
}

#[derive(Clone)]
pub struct Monitor {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    app: AppHandle,
    dir: PathBuf,
    log_path: PathBuf,
}

impl Monitor {
    pub fn new(app: AppHandle, dir: PathBuf, log_path: PathBuf, settings: Settings) -> Self {
        let snap = Snapshot {
            status: Status::Checking,
            detail: String::new(),
            last_check: 0,
            logins: 0,
            events: Vec::new(),
        };
        // 密码在监控线程里读取：读钥匙串可能弹出授权框，不能阻塞启动
        let inner = Inner { snap, settings, password: None, bad_password: false, request: None };
        Self { inner: Arc::new((Mutex::new(inner), Condvar::new())), app, dir, log_path }
    }

    pub fn log_path(&self) -> &PathBuf {
        &self.log_path
    }

    /// 追加一行到日志文件，超过上限时把旧日志转存为 .old。不记录密码。
    fn log(&self, line: &str) {
        if let Some(dir) = self.log_path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if fs::metadata(&self.log_path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            let _ = fs::rename(&self.log_path, self.log_path.with_extension("log.old"));
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.log_path) {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let _ = writeln!(f, "{now} {line}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.0.lock().unwrap()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().snap.clone()
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    pub fn has_password(&self) -> bool {
        self.lock().password.is_some()
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<(), String> {
        let mut g = self.lock();
        f(&mut g.settings);
        g.settings.save(&self.settings_path()).map_err(|e| format!("保存设置失败：{e}"))?;
        drop(g);
        self.request(false);
        Ok(())
    }

    pub fn save_account(&self, username: String, password: String) -> Result<(), String> {
        config::store_password(&username, &password)?;
        let old = self.settings().username;
        if !old.is_empty() && old != username {
            config::delete_password(&old);
        }
        {
            let mut g = self.lock();
            g.password = Some(password);
            g.bad_password = false;
        }
        self.update_settings(|s| s.username = username)?;
        self.request(true);
        Ok(())
    }

    pub fn clear_account(&self) -> Result<(), String> {
        let old = self.settings().username;
        if !old.is_empty() {
            config::delete_password(&old);
        }
        self.lock().password = None;
        self.update_settings(|s| s.username.clear())
    }

    /// 唤醒监控线程立即检测一次。
    pub fn request(&self, forced: bool) {
        let mut g = self.lock();
        g.request = Some(forced || g.request == Some(true));
        self.inner.1.notify_one();
    }

    pub fn start(&self) {
        self.log(&format!("启动 v{}", env!("CARGO_PKG_VERSION")));
        let m = self.clone();
        thread::Builder::new()
            .name("monitor".into())
            .spawn(move || m.run())
            .expect("无法启动监控线程");
    }

    fn run(self) {
        let username = self.settings().username;
        if !username.is_empty() {
            let password = config::load_password(&username);
            if password.is_none() {
                self.log("无法从系统凭据库读取密码");
            }
            let mut g = self.lock();
            if g.password.is_none() {
                g.password = password;
            }
            drop(g);
            let _ = self.app.emit("settings", ());
        }

        let portal = Portal::new();
        let mut last_fp: Option<Vec<String>> = None;
        let mut last_wall = SystemTime::now();
        let mut next_check = Instant::now();
        let mut failures = 0u32;
        let mut settle = 0u32;

        loop {
            let request = self.lock().request.take();

            let fp = fingerprint();
            let diff = last_fp.as_ref().filter(|old| **old != fp).map(|old| describe_change(old, &fp));
            let changed = diff.is_some();
            last_fp = Some(fp);
            // 睡眠期间单调时钟可能不走，用墙上时间的跳变判断是否刚唤醒
            let now = SystemTime::now();
            let resumed = now.duration_since(last_wall).is_ok_and(|d| d > TICK * 10);
            last_wall = now;

            if let Some(diff) = diff {
                self.push_event("info", "网络发生变化".into());
                self.log(&format!("  {diff}"));
            } else if resumed {
                self.push_event("info", "系统从睡眠中唤醒".into());
            }
            if changed || resumed {
                settle = SETTLE_TRIES;
            }

            if request.is_some() || changed || resumed || Instant::now() >= next_check {
                let mut delay = self.check(&portal, request == Some(true), &mut failures);
                let status = self.lock().snap.status;
                if status == Status::Online {
                    settle = 0;
                } else if settle > 0 && failures == 0 {
                    settle -= 1;
                    delay = SETTLE_INTERVAL;
                }
                next_check = Instant::now() + delay;
            }

            let g = self.lock();
            if g.request.is_none() {
                let _ = self.inner.1.wait_timeout(g, TICK);
            }
        }
    }

    /// 检测一次，必要时登录；返回距离下次检测的间隔。
    fn check(&self, portal: &Portal, forced: bool, failures: &mut u32) -> Duration {
        let (settings, password) = {
            let g = self.lock();
            (g.settings.clone(), g.password.clone())
        };

        let portal_url = match portal.probe() {
            Probe::Online => {
                *failures = 0;
                self.set(Status::Online, "");
                return ONLINE_INTERVAL;
            }
            Probe::Captive(url) => url,
            Probe::Unreachable => None,
        };
        if let Err(e) = portal.on_campus(&settings.gateway) {
            *failures = 0;
            if self.lock().snap.status != Status::OffCampus {
                self.log(&format!("无法访问认证网关：{e}"));
            }
            self.set(Status::OffCampus, "当前网络不是北邮校园网");
            return OFFLINE_INTERVAL;
        }
        if !settings.auto_login && !forced {
            self.set(Status::NeedsLogin, "自动登录已关闭");
            return OFFLINE_INTERVAL;
        }
        let Some(password) = password.filter(|_| !settings.username.is_empty()) else {
            self.set(Status::NoAccount, "请先填写学号和密码");
            return OFFLINE_INTERVAL;
        };
        if self.lock().bad_password && !forced {
            self.set(Status::Failed, BAD_PASSWORD_HINT);
            return OFFLINE_INTERVAL;
        }

        self.set(Status::LoggingIn, "");
        self.log(&format!("门户跳转地址：{}", portal_url.as_deref().unwrap_or("（无）")));
        let result =
            portal.login(&settings.gateway, portal_url.as_deref(), &settings.username, &password);
        let trace = match &result {
            Ok(trace) => trace,
            Err(e) => &e.trace,
        };
        for step in trace {
            self.log(&format!("  {step}"));
        }
        match result {
            Ok(_) => {
                *failures = 0;
                {
                    let mut g = self.lock();
                    g.snap.logins += 1;
                    g.bad_password = false;
                }
                self.set(Status::Online, "");
                if settings.notify {
                    self.notify("校园网已自动登录", &format!("账号 {}", settings.username));
                }
                ONLINE_INTERVAL
            }
            Err(e) if e.bad_credentials => {
                *failures = 0;
                self.lock().bad_password = true;
                if let Some(html) = &e.html {
                    let _ = fs::write(self.dir.join("last-login-response.html"), html);
                }
                self.log(&format!("  网关拒绝了账号密码（{}），暂停自动登录", e.message));
                if settings.notify {
                    self.notify("校园网密码错误", "已暂停自动登录，请在主界面更新密码");
                }
                self.set(Status::Failed, BAD_PASSWORD_HINT);
                OFFLINE_INTERVAL
            }
            Err(e) => {
                *failures += 1;
                if let Some(html) = &e.html {
                    let _ = fs::write(self.dir.join("last-login-response.html"), html);
                }
                if *failures == 1 && settings.notify {
                    self.notify("校园网自动登录失败", &e.message);
                }
                self.set(Status::Failed, &e.message);
                // 30s, 60s, 120s … 最长 10 分钟，避免密码错误时频繁请求
                let secs = (30u64 << (*failures - 1).min(5)).min(MAX_BACKOFF);
                self.log(&format!("  连续失败 {} 次，{secs} 秒后重试", *failures));
                Duration::from_secs(secs)
            }
        }
    }

    fn set(&self, status: Status, detail: &str) {
        let mut logged = None;
        let (snap, changed) = {
            let mut g = self.lock();
            let prev = g.snap.status;
            let changed = prev != status;
            if changed && status != Status::Checking {
                let (level, text) = match status {
                    Status::Online if prev == Status::LoggingIn => ("ok", "登录成功".into()),
                    Status::Online => ("ok", "已联网".into()),
                    Status::LoggingIn => ("info", "检测到未认证，正在登录".into()),
                    Status::Failed => ("err", format!("登录失败：{detail}")),
                    Status::NoAccount => ("warn", "未设置账号，无法自动登录".into()),
                    _ => ("info", status.label().into()),
                };
                logged = Some(text.clone());
                push_event(&mut g, level, text);
            }
            g.snap.status = status;
            g.snap.detail = detail.into();
            g.snap.last_check = now_secs();
            (g.snap.clone(), changed)
        };
        if let Some(text) = logged {
            self.log(&text);
        }
        if changed {
            tray::refresh(&self.app, status);
        }
        let _ = self.app.emit("state", &snap);
    }

    fn push_event(&self, level: &'static str, text: String) {
        self.log(&text);
        let snap = {
            let mut g = self.lock();
            push_event(&mut g, level, text);
            g.snap.clone()
        };
        let _ = self.app.emit("state", &snap);
    }

    fn notify(&self, title: &str, body: &str) {
        let _ = self.app.notification().builder().title(title).body(body).show();
    }
}

fn push_event(g: &mut Inner, level: &'static str, text: String) {
    g.snap.events.insert(0, Event { time: now_secs(), level, text });
    g.snap.events.truncate(MAX_EVENTS);
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// 本机物理网卡的 IPv4 地址集合。
/// 不纳入 IPv6（临时地址会周期性轮换）、169.254 自分配地址（DHCP 失败时会反复出现又消失）
/// 和虚拟网卡（VPN、虚拟机等的地址变化不代表换了网络），避免误判网络变化。
fn fingerprint() -> Vec<String> {
    let mut addrs: Vec<String> =
        http::physical_ipv4().into_iter().map(|i| format!("{}={}", i.name, i.ip())).collect();
    addrs.sort();
    addrs.dedup();
    addrs
}

/// 例："+en0=10.129.1.2 -en0=10.128.3.4"
fn describe_change(old: &[String], new: &[String]) -> String {
    let added = new.iter().filter(|a| !old.contains(a)).map(|a| format!("+{a}"));
    let removed = old.iter().filter(|a| !new.contains(a)).map(|a| format!("-{a}"));
    added.chain(removed).collect::<Vec<_>>().join(" ")
}
