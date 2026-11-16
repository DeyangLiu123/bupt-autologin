use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

/// 无线网 (buptportal) 的认证网关
pub const DEFAULT_GATEWAY: &str = "http://10.3.8.216";
const KEYRING_SERVICE: &str = "bupt-autologin";

/// 非敏感设置，存为 JSON；密码只放在系统凭据库里。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub username: String,
    pub gateway: String,
    pub auto_login: bool,
    pub notify: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            username: String::new(),
            gateway: DEFAULT_GATEWAY.into(),
            auto_login: true,
            notify: true,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self).unwrap())
    }
}

pub fn load_password(user: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, user)
        .ok()?
        .get_password()
        .ok()
}

pub fn store_password(user: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, user)
        .and_then(|e| e.set_password(password))
        .map_err(|e| format!("无法保存到系统凭据库：{e}"))
}

pub fn delete_password(user: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, user) {
        let _ = entry.delete_credential();
    }
}
