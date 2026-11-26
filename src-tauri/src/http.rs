//! 极简 HTTP/1.1 客户端，只支持纯 HTTP 和 IPv4（网关和探测地址都是纯 HTTP，校园网按 IPv4 认证）。
//!
//! macOS 检测到 Wi-Fi 需要网页认证时，会把这块网卡的路由优先级降为 Never，直到在系统认证弹窗里登录为止。
//! 这期间系统没有默认路由，普通连接会立即失败，只有绑定了网卡的连接才能访问网关（系统认证弹窗就是这样做的）。
//! 现成的 HTTP 库不能指定网卡，所以这里自己发请求：在 macOS 上把连接绑定到物理网卡（IP_BOUND_IF）。

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs},
    num::NonZeroU32,
    sync::Mutex,
    time::Duration,
};

const MAX_RESPONSE: usize = 1 << 20;
/// 虚拟网卡（VPN、虚拟机、容器、热点、AirDrop 等）
const VIRTUAL_IFACE_PREFIXES: &[&str] = &[
    "utun", "tun", "tap", "wg", "tailscale", "zt", "bridge", "br-", "docker", "veth", "virbr",
    "vmnet", "vboxnet", "vethernet", "awdl", "llw", "anpi", "ap", "gif", "stf",
];

/// 物理网卡上的 IPv4 地址，不含回环、169.254 自分配地址和虚拟网卡。
pub fn physical_ipv4() -> Vec<if_addrs::Interface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| match i.ip() {
            IpAddr::V4(ip) => !ip.is_loopback() && !ip.is_link_local(),
            IpAddr::V6(_) => false,
        })
        .filter(|i| {
            let name = i.name.to_ascii_lowercase();
            !VIRTUAL_IFACE_PREFIXES.iter().any(|p| name.starts_with(p))
        })
        .collect()
}

pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct Client {
    user_agent: &'static str,
    connect_timeout: Duration,
    timeout: Duration,
    /// (主机, 名称, 值)
    cookies: Mutex<Vec<(String, String, String)>>,
}

impl Client {
    pub fn new(user_agent: &'static str, connect_timeout: Duration, timeout: Duration) -> Self {
        Self { user_agent, connect_timeout, timeout, cookies: Mutex::default() }
    }

    pub fn get(&self, url: &str) -> io::Result<Response> {
        self.send("GET", url, &[], None, self.timeout)
    }

    /// 提交表单；timeout 是等待响应的时间，网关处理登录可能比普通请求慢。
    pub fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        form: &[(&str, &str)],
        timeout: Duration,
    ) -> io::Result<Response> {
        let body = form.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v)));
        self.send("POST", url, headers, Some(body.collect::<Vec<_>>().join("&")), timeout)
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<String>,
        timeout: Duration,
    ) -> io::Result<Response> {
        let (host, path) = split_url(url)?;
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: close\r\n",
            self.user_agent
        );
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        let cookie = self.cookie_header(host);
        if !cookie.is_empty() {
            req += &format!("Cookie: {cookie}\r\n");
        }
        if let Some(body) = &body {
            req += &format!(
                "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
                body.len()
            );
        }
        req += "\r\n";
        req += body.as_deref().unwrap_or("");

        let mut stream = self.connect(host)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(req.as_bytes()).map_err(timed_out)?;
        let resp = read_response(&mut stream).map_err(timed_out)?;
        self.store_cookies(host, &resp);
        Ok(resp)
    }

    /// 依次尝试各个地址和网卡，返回第一个连上的。
    fn connect(&self, host: &str) -> io::Result<TcpStream> {
        let target = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
        let addrs = target.to_socket_addrs()?.filter_map(|a| match a {
            SocketAddr::V4(a) => Some(a),
            SocketAddr::V6(_) => None,
        });
        let mut last = io::Error::other(format!("{host} 没有 IPv4 地址"));
        for addr in addrs {
            for iface in interfaces_for(&addr) {
                match self.open(addr, iface) {
                    Ok(stream) => return Ok(stream),
                    Err(e) => last = e,
                }
            }
        }
        Err(last)
    }

    fn open(&self, addr: SocketAddrV4, iface: Option<NonZeroU32>) -> io::Result<TcpStream> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        #[cfg(target_os = "macos")]
        socket.bind_device_by_index_v4(iface)?;
        #[cfg(not(target_os = "macos"))]
        let _ = iface;
        socket.connect_timeout(&SockAddr::from(addr), self.connect_timeout)?;
        socket.set_write_timeout(Some(self.timeout))?;
        Ok(socket.into())
    }

    fn cookie_header(&self, host: &str) -> String {
        let jar = self.cookies.lock().unwrap();
        let pairs = jar.iter().filter(|(h, ..)| h == host).map(|(_, k, v)| format!("{k}={v}"));
        pairs.collect::<Vec<_>>().join("; ")
    }

    fn store_cookies(&self, host: &str, resp: &Response) {
        let mut jar = self.cookies.lock().unwrap();
        let set = resp.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("Set-Cookie"));
        for (_, value) in set {
            let pair = value.split(';').next().unwrap_or("");
            let Some((name, value)) = pair.split_once('=') else { continue };
            let name = name.trim();
            jar.retain(|(h, k, _)| !(h == host && k == name));
            jar.push((host.to_string(), name.to_string(), value.trim().to_string()));
        }
    }
}

/// macOS 上依次绑定各块物理网卡；找不到物理网卡（或目标是本机）时按系统路由连接。
fn interfaces_for(addr: &SocketAddrV4) -> Vec<Option<NonZeroU32>> {
    let mut ifaces: Vec<Option<NonZeroU32>> = Vec::new();
    if cfg!(target_os = "macos") && !addr.ip().is_loopback() {
        ifaces = physical_ipv4().iter().filter_map(|i| i.index.and_then(NonZeroU32::new)).map(Some).collect();
        ifaces.sort();
        ifaces.dedup();
    }
    if ifaces.is_empty() {
        ifaces.push(None);
    }
    ifaces
}

/// "http://host:port/path?q" → ("host:port", "/path?q")
fn split_url(url: &str) -> io::Result<(&str, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("只支持 http 地址：{url}")))?;
    let i = rest.find(['/', '?']).unwrap_or(rest.len());
    let path = &rest[i..];
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    Ok((&rest[..i], path))
}

/// application/x-www-form-urlencoded 编码
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out += &format!("%{b:02X}"),
        }
    }
    out
}

/// macOS 上读写超时报的是 EAGAIN（"Resource temporarily unavailable (os error 35)"），统一成超时。
fn timed_out(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "等待响应超时")
        }
        _ => e,
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_response(stream: &mut impl Read) -> io::Result<Response> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf)?;
        raw.extend_from_slice(&buf[..n]);
        if let Some(resp) = parse(&raw, n == 0)? {
            return Ok(resp);
        }
        if raw.len() > MAX_RESPONSE {
            return Err(invalid("响应过大"));
        }
    }
}

/// 解析完整的响应；数据还不够时返回 None。eof 表示连接已关闭，此时数据不完整就报错。
fn parse(raw: &[u8], eof: bool) -> io::Result<Option<Response>> {
    let Some(end) = find(raw, b"\r\n\r\n") else {
        return if eof { Err(invalid("响应头不完整")) } else { Ok(None) };
    };
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("无效的状态行"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut resp = Response { status, headers, body: String::new() };

    let rest = &raw[end + 4..];
    let body = if status == 204 || status == 304 {
        Some(Vec::new())
    } else if resp.header("Transfer-Encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        dechunk(rest)
    } else if let Some(len) = resp.header("Content-Length").and_then(|v| v.parse::<usize>().ok()) {
        (rest.len() >= len).then(|| rest[..len].to_vec())
    } else {
        eof.then(|| rest.to_vec())
    };
    match body {
        Some(body) => {
            resp.body = String::from_utf8_lossy(&body).into_owned();
            Ok(Some(resp))
        }
        None if eof => Err(invalid("响应正文不完整")),
        None => Ok(None),
    }
}

/// 解码 chunked 正文；数据还不完整时返回 None。
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find(data, b"\r\n")?;
        let line = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_and_waits_for_more() {
        let raw = b"HTTP/1.1 302 Found\r\nLocation: /index\r\nContent-Length: 5\r\n\r\nabc";
        assert!(parse(raw, false).unwrap().is_none());
        assert!(parse(raw, true).is_err());
        let resp = parse(b"HTTP/1.1 302 Found\r\nlocation: /index\r\nContent-Length: 3\r\n\r\nabc", false)
            .unwrap()
            .unwrap();
        assert_eq!(resp.status, 302);
        assert_eq!(resp.header("Location"), Some("/index"));
        assert_eq!(resp.body, "abc");
    }

    #[test]
    fn parses_chunked_and_unsized() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=1\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse(raw, false).unwrap().unwrap().body, "abcde");
        assert!(parse(&raw[..raw.len() - 7], false).unwrap().is_none());
        let raw = b"HTTP/1.0 200 OK\r\n\r\nhello";
        assert!(parse(raw, false).unwrap().is_none());
        assert_eq!(parse(raw, true).unwrap().unwrap().body, "hello");
        assert_eq!(parse(b"HTTP/1.1 204 No Content\r\n\r\n", false).unwrap().unwrap().status, 204);
    }

    #[test]
    fn reports_timeout_when_server_is_silent() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/login", listener.local_addr().unwrap());
        // 收下连接但不回复
        std::thread::spawn(move || {
            let _conns: Vec<_> = listener.incoming().collect();
        });
        let client = Client::new("test", Duration::from_secs(1), Duration::from_secs(5));
        let Err(err) = client.post_form(&url, &[], &[("a", "b")], Duration::from_millis(200)) else {
            panic!("应当超时");
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "等待响应超时");
    }

    #[test]
    fn encodes_form() {
        assert_eq!(encode("p@ss word&=中"), "p%40ss+word%26%3D%E4%B8%AD");
        assert_eq!(encode("a-b_c.d*9"), "a-b_c.d*9");
    }

    #[test]
    fn splits_url() {
        assert_eq!(split_url("http://10.3.8.216").unwrap(), ("10.3.8.216", "/".into()));
        assert_eq!(
            split_url("http://127.0.0.1:80/index?a=1").unwrap(),
            ("127.0.0.1:80", "/index?a=1".into())
        );
        assert_eq!(split_url("http://a.b?x=1").unwrap(), ("a.b", "/?x=1".into()));
        assert!(split_url("https://a.b/").is_err());
    }
}
