//! 与认证网关和外网探测地址的 HTTP 交互。所有请求直连，不走系统代理。

use crate::http::{Client, Response};
use std::{io, time::Duration};

const APPLE_PROBE: &str = "http://captive.apple.com/hotspot-detect.html";
const PROBE_204: &str = "http://connect.rom.miui.com/generate_204";
// 部分网关会拒绝非浏览器的请求，按浏览器的方式提交
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
/// 提交后等待网关放行的检测次数和间隔
const VERIFY_TRIES: u32 = 4;
/// 网页最多等待拨号 30 秒
const DIAL_TRIES: u32 = 15;
/// 拨号完成后网关可能过几十秒才放行，多等一会
const DIALED_VERIFY_TRIES: u32 = 15;
const VERIFY_INTERVAL: Duration = Duration::from_secs(2);

const ERROR_HINTS: &[&str] = &[
    "密码错误", "密码不正确", "用户不存在", "账号不存在", "欠费", "余额不足",
    "在线数", "上限", "锁定", "停用", "错误", "失败",
];

pub enum Probe {
    Online,
    /// 被门户拦截；附带拦截时跳转到的登录页地址（可能带有本机 IP/MAC 参数）
    Captive(Option<String>),
    Unreachable,
}

#[derive(Debug)]
pub struct Login {
    pub trace: Vec<String>,
    /// 网关显示本机早已认证，没有提交账号密码
    pub already: bool,
}

pub struct LoginError {
    pub message: String,
    /// 网关明确拒绝了账号密码；此时不应自动重试，以免账号被锁
    pub bad_credentials: bool,
    /// 网关最后返回的页面，便于排查
    pub html: Option<String>,
    pub trace: Vec<String>,
}

pub struct Portal {
    http: Client,
}

impl Portal {
    pub fn new() -> Self {
        Self { http: Client::new(USER_AGENT, Duration::from_secs(3), Duration::from_secs(6)) }
    }

    /// 任一探测地址返回预期内容即视为已联网；被门户劫持时记下跳转地址。
    pub fn probe(&self) -> Probe {
        let mut reached = false;
        let mut portal = None;
        for (url, expect_204) in [(APPLE_PROBE, false), (PROBE_204, true)] {
            let Ok(resp) = self.http.get(url) else { continue };
            reached = true;
            let online = if expect_204 {
                resp.status == 204
            } else {
                resp.body.contains("<TITLE>Success</TITLE>")
            };
            if online {
                return Probe::Online;
            }
            if portal.is_none() {
                portal = resp.header("Location").map(str::to_string).or_else(|| find_url(&resp.body));
            }
        }
        if reached {
            Probe::Captive(portal)
        } else {
            Probe::Unreachable
        }
    }

    pub fn is_online(&self) -> bool {
        matches!(self.probe(), Probe::Online)
    }

    /// 能访问认证网关才说明连着校园网，否则不发登录请求。
    pub fn on_campus(&self, gateway: &str) -> io::Result<()> {
        self.http.get(&format!("{gateway}/")).map(drop)
    }

    /// 按网页的流程登录：打开登录页拿会话 cookie → 提交表单 → 轮询 /dial 等待拨号完成。
    pub fn login(
        &self,
        gateway: &str,
        portal: Option<&str>,
        user: &str,
        password: &str,
    ) -> Result<Login, LoginError> {
        let mut trace = Vec::new();
        let entry = portal
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .map(str::to_string)
            .unwrap_or_else(|| format!("{gateway}/index"));
        let base = origin(&entry).to_string();
        let fail = |message: String, html: Option<String>, trace: Vec<String>| {
            Err(LoginError { bad_credentials: is_bad_credentials(&message), message, html, trace })
        };

        let page = match self.http.get(&entry) {
            Ok(r) => {
                trace.push(format!("GET {entry} → {}", describe(&r)));
                r.body
            }
            Err(e) => return fail(format!("无法打开登录页（{e}）"), None, trace),
        };
        // 已认证时再提交，网关会误报"用户名或密码错误 (0x01000004)"。
        // 常见于合盖后系统短暂唤醒、Wi-Fi 刚重连时外网探测失败
        if already_logged_in(&page) {
            trace.push("网关显示本机已认证，不再提交登录".into());
            for _ in 0..DIALED_VERIFY_TRIES {
                if self.is_online() {
                    return Ok(Login { trace, already: true });
                }
                std::thread::sleep(VERIFY_INTERVAL);
            }
            return fail("网关显示已认证，但暂时还不能上网".into(), Some(page), trace);
        }

        let resp = match self.http.post_form(
            &format!("{base}/login"),
            &[("Referer", &entry), ("Origin", &base)],
            &[("user", user), ("pass", password)],
        ) {
            Ok(r) => r,
            Err(e) => return fail(format!("无法连接认证网关（{e}）"), None, trace),
        };
        trace.push(format!("POST {base}/login → {}", describe(&resp)));
        let location = resp.header("Location").map(|loc| resolve(&base, loc));
        let mut html = resp.body;

        // 失败时网关会跳回登录页并在 errmsg 参数里给出原因
        if let Some(next) = location {
            if let Some(err) = query_param(&next, "errmsg") {
                return fail(gateway_error(&err), None, trace);
            }
            if let Ok(r) = self.http.get(&next) {
                trace.push(format!("GET {next} → {}", describe(&r)));
                html = r.body;
            }
        }
        trace.push(format!("结果页 {} 字节", html.len()));
        // 账号密码被拒绝时，网关把登录页连同错误框一起返回，不会进入拨号
        if let Some(msg) = error_box(&html) {
            return fail(msg, Some(html), trace);
        }

        // 网页提交后每 2 秒查询一次 /dial，code 为 0 表示拨号完成
        let mut dial_msg = None;
        let mut dialed = false;
        for _ in 0..DIAL_TRIES {
            let reply = self
                .http
                .get(&format!("{base}/dial"))
                .ok()
                .and_then(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok());
            let code = reply.as_ref().and_then(|v| v["code"].as_i64());
            if code == Some(0) {
                trace.push("拨号完成".into());
                dialed = true;
                break;
            }
            dial_msg = reply.and_then(|v| v["msg"].as_str().map(str::to_string));
            std::thread::sleep(VERIFY_INTERVAL);
        }

        // 网关处理完需要一点时间放行
        let tries = if dialed { DIALED_VERIFY_TRIES } else { VERIFY_TRIES };
        for _ in 0..tries {
            if self.is_online() {
                return Ok(Login { trace, already: false });
            }
            std::thread::sleep(VERIFY_INTERVAL);
        }
        // 拨号成功页的脚本里带有 alert('拨号失败…') 模板，此时不能从页面里找原因
        let message = match (dialed, dial_msg) {
            (true, _) => "拨号已完成，但暂时还不能上网".into(),
            (false, Some(msg)) => format!("拨号失败：{msg}"),
            (false, None) => explain(&html),
        };
        fail(message, Some(html), trace)
    }
}

fn describe(r: &Response) -> String {
    let mut s = r.status.to_string();
    if let Some(loc) = r.header("Location") {
        s += &format!(" Location={loc}");
    }
    if r.header("Set-Cookie").is_some() {
        s += " (Set-Cookie)";
    }
    s
}

fn is_bad_credentials(message: &str) -> bool {
    ["密码错误", "用户名或密码", "用户不存在", "账号不存在", "0x01000004"]
        .iter()
        .any(|k| message.contains(k))
}

/// 已认证时登录页换成"正在拨号 / 登录成功"页：有注销按钮，没有账号输入框。
fn already_logged_in(html: &str) -> bool {
    html.contains("login-success") && !html.contains("name=\"user\"")
}

/// 网关错误页里的 <div class="ui error message">…</div> 文本。
fn error_box(html: &str) -> Option<String> {
    let start = html.find("error message")?;
    let body = &html[start..];
    let body = &body[body.find('>')? + 1..];
    let text = visible_text(&body[..body.find("</div>")?]);
    (!text.is_empty()).then_some(text)
}

fn gateway_error(err: &str) -> String {
    match err {
        "Access denied" => "网关拒绝访问（Access denied）".into(),
        _ => err.to_string(),
    }
}

/// 取 URL 查询参数并做百分号解码。
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = &url[url.find('?')? + 1..];
    let value = query.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))?;
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (decoded, bytes[i]) {
            (Some(b), _) => {
                out.push(b);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, b) => out.push(b),
        }
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// "http://host:port/path" → "http://host:port"
fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    match url[after_scheme..].find('/') {
        Some(i) => &url[..after_scheme + i],
        None => url,
    }
}

fn resolve(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if location.starts_with('/') {
        format!("{base}{location}")
    } else {
        format!("{base}/{location}")
    }
}

/// 门户劫持页常用 JS 或 meta refresh 跳转，从页面里找出登录页地址。
fn find_url(body: &str) -> Option<String> {
    let mut rest = body;
    while let Some(pos) = rest.find("http") {
        let candidate = &rest[pos..];
        rest = &rest[pos + 4..];
        if !(candidate.starts_with("http://") || candidate.starts_with("https://")) {
            continue;
        }
        let end = candidate
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace())
            .unwrap_or(candidate.len());
        let url = &candidate[..end];
        if !url.contains("captive.apple.com") && !url.contains("miui.com") && !url.contains("w3.org") {
            return Some(url.to_string());
        }
    }
    None
}

/// 从网关返回的页面里尽量找出失败原因。
fn explain(html: &str) -> String {
    if let Some(msg) = alert_message(html) {
        return msg;
    }
    let text = visible_text(html);
    for hint in ERROR_HINTS {
        if let Some(pos) = text.find(hint) {
            let chars: Vec<char> = text.chars().collect();
            let at = text[..pos].chars().count();
            let start = at.saturating_sub(20);
            let end = (at + 30).min(chars.len());
            return chars[start..end].iter().collect::<String>().trim().to_string();
        }
    }
    "登录后仍无法联网，网关未给出原因".into()
}

/// 取页面里的 alert('...') 文本，跳过登录页自带的表单校验提示，
/// 以及后面还要拼接变量的提示模板（如 alert('拨号失败…' + res.msg)）。
fn alert_message(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(pos) = rest.find("alert(") {
        rest = &rest[pos + 6..];
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let body = &rest[1..];
        let Some(end) = body.find(quote) else { continue };
        let msg = body[..end].trim();
        let concatenated = body[end + 1..].trim_start().starts_with('+');
        if !msg.is_empty() && !concatenated && !msg.starts_with("请输入") && !msg.contains("验证码") {
            return Some(msg.to_string());
        }
    }
    None
}

fn visible_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        out.push(' ');
        let tag = &rest[lt..];
        let lower = tag.get(..7).unwrap_or("").to_ascii_lowercase();
        let skip_to = if lower.starts_with("<script") || lower.starts_with("<style") {
            let close = if lower.starts_with("<script") { "</script>" } else { "</style>" };
            tag.to_ascii_lowercase().find(close).map(|i| i + close.len())
        } else {
            tag.find('>').map(|i| i + 1)
        };
        match skip_to {
            Some(n) => rest = &tag[n..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_alert() {
        assert_eq!(explain("<script>alert('密码错误');</script>"), "密码错误");
    }

    #[test]
    fn skips_form_validation_alerts() {
        let html = "<script>if(!u){alert('请输入账号')} alert(ret.message)</script><p>登录成功</p>";
        assert_eq!(alert_message(html), None);
    }

    #[test]
    fn skips_alert_templates() {
        let html = "<script>if (time < 30) {} else { alert('拨号失败！请联系运营商。\\n错误信息:' + res.msg) }</script>";
        assert_eq!(alert_message(html), None);
    }

    #[test]
    fn explains_text_hint() {
        let html = "<html><style>a{}</style><body><p>认证失败：账号欠费</p></body></html>";
        assert!(explain(html).contains("欠费"));
    }

    #[test]
    fn strips_tags_and_scripts() {
        let html = "<div>你好</div><script>var x = '<b>';</script><span>世界</span>";
        assert_eq!(visible_text(html), "你好 世界");
    }

    #[test]
    fn finds_portal_url_in_redirect_page() {
        let html = "<html><script>top.self.location.href='http://10.3.8.211/index?ip=10.129.1.2&mac=aa'</script></html>";
        assert_eq!(find_url(html).as_deref(), Some("http://10.3.8.211/index?ip=10.129.1.2&mac=aa"));
    }

    #[test]
    fn reads_error_box() {
        let html = r#"<form><div class="msg">
                        <div class="ui error message">
                            用户名或密码错误 (0x01000004)
                        </div>
                    </div><p>请修改后使用！</p></form>"#;
        assert_eq!(error_box(html).as_deref(), Some("用户名或密码错误 (0x01000004)"));
        assert!(is_bad_credentials("用户名或密码错误 (0x01000004)"));
        assert!(!is_bad_credentials("网关拒绝访问（Access denied）"));
        assert_eq!(error_box("<form><input name=user></form>"), None);
    }

    #[test]
    fn detects_logged_in_page() {
        let online = r#"<div class="login-success" style="display: none;"><h3 class="center title">登录成功</h3>
            <button onclick="logout()">注销</button></div>
            <div class="login-process"><h3 class="center title">正在拨号，请等待...</h3></div>"#;
        let form = r#"<form id="form" method="POST" action="login">
            <input placeholder="输入账号" type="text" class="el-input__inner" name="user"></form>"#;
        assert!(already_logged_in(online));
        assert!(!already_logged_in(form));
        assert!(!already_logged_in("<form></form>"));
    }

    #[test]
    fn reads_errmsg_param() {
        let url = "http://10.3.8.216/index?errmsg=Access%20denied";
        assert_eq!(query_param(url, "errmsg").as_deref(), Some("Access denied"));
        assert_eq!(query_param("http://a/index?x=%E4%BD%A0+b", "x").as_deref(), Some("你 b"));
        assert_eq!(query_param("http://a/index?x=50%", "x").as_deref(), Some("50%"));
        assert_eq!(query_param("http://a/index", "errmsg"), None);
    }

    #[test]
    fn origin_and_resolve() {
        assert_eq!(origin("http://10.3.8.211/index?a=1"), "http://10.3.8.211");
        assert_eq!(origin("http://10.3.8.211"), "http://10.3.8.211");
        assert_eq!(resolve("http://10.3.8.211", "/index"), "http://10.3.8.211/index");
        assert_eq!(resolve("http://10.3.8.211", "http://a.b/c"), "http://a.b/c");
    }
}

/// 用本地端口模拟网关，验证登录和拨号轮询的流程。
#[cfg(test)]
mod flow_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// 极简 HTTP 服务器：按 handler 返回 (状态行, 额外头, 正文)，并记录每个请求。
    fn serve(
        log: Log,
        name: &'static str,
        handler: impl Fn(&str, &str, &str) -> (&'static str, String, String) + Send + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let (mut len, mut cookie) = (0, String::new());
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("cookie:") {
                        cookie = h[7..].trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                let path = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                log.lock().unwrap().push(format!("{name} {path} cookie={}", !cookie.is_empty()));
                let (status, headers, text) = handler(&path, &cookie, &body);
                let mut out = stream;
                let _ = write!(
                    out,
                    "HTTP/1.1 {status}\r\n{headers}Set-Cookie: SessionId={name}1; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        addr
    }

    /// 最后一步要确认外网连通，需要联网，所以默认不跑：cargo test -- --ignored
    #[test]
    #[ignore]
    fn logs_in_and_waits_for_dial() {
        let log: Log = Arc::default();
        let dial_calls = Arc::new(Mutex::new(0));
        let dials = dial_calls.clone();
        let gw = serve(log.clone(), "GW", move |path, _, body| match path {
            "POST /login" => {
                assert_eq!(body, "user=2021000000&pass=p%40ss");
                ("200 OK", String::new(), "<h3>正在拨号，请等待...</h3>".into())
            }
            "GET /dial" => {
                let mut n = dials.lock().unwrap();
                *n += 1;
                let code = if *n < 3 { 1 } else { 0 };
                ("200 OK", String::new(), format!("{{\"code\": {code}, \"msg\": \"dialing\"}}"))
            }
            _ => ("200 OK", String::new(), "<form></form>".into()),
        });

        let result = Portal::new().login(&gw, None, "2021000000", "p@ss");
        if let Err(e) = &result {
            panic!("登录失败：{}；trace={:?}", e.message, e.trace);
        }
        assert_eq!(
            *log.lock().unwrap(),
            [
                "GW GET /index cookie=false",
                "GW POST /login cookie=true",
                "GW GET /dial cookie=true",
                "GW GET /dial cookie=true",
                "GW GET /dial cookie=true",
            ]
        );
    }

    #[test]
    fn stops_on_wrong_password_without_dialing() {
        let log: Log = Arc::default();
        let gw = serve(log.clone(), "GW", |path, _, _| match path {
            "POST /login" => (
                "200 OK",
                String::new(),
                "<div class=\"ui error message\">用户名或密码错误 (0x01000004)</div>".into(),
            ),
            _ => ("200 OK", String::new(), "<form></form>".into()),
        });

        let err = Portal::new().login(&gw, None, "2021000000", "x").expect_err("应当失败");
        assert_eq!(err.message, "用户名或密码错误 (0x01000004)");
        assert!(err.bad_credentials);
        assert_eq!(*log.lock().unwrap(), ["GW GET /index cookie=false", "GW POST /login cookie=true"]);
    }

    #[test]
    fn reports_errmsg_without_dialing() {
        let log: Log = Arc::default();
        let gw = serve(log.clone(), "GW", |path, _, _| match path {
            "POST /login" => (
                "302 Found",
                "Location: /index?errmsg=Access%20denied\r\n".into(),
                String::new(),
            ),
            _ => ("200 OK", String::new(), "<form></form>".into()),
        });

        let err = Portal::new().login(&gw, None, "2021000000", "x").expect_err("应当失败");
        assert_eq!(err.message, "网关拒绝访问（Access denied）");
        assert_eq!(*log.lock().unwrap(), ["GW GET /index cookie=false", "GW POST /login cookie=true"]);
    }
}
