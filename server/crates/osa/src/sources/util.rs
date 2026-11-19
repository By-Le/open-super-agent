//! 事件来源共用的小工具：联网抓取（挡内网、限大小、自己跟跳转）、网页转文字、截断。对应 osa/sources/_util.py。

use crate::{config, py};
use regex::Regex;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;

pub const UA: &str = "Mozilla/5.0 (compatible; open-super-agent events)";
pub const MAX_REDIRECTS: usize = 5;

/// 抓到的网页
pub struct Resp {
    pub status: u16,
    pub reason: String,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
    pub url: String,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, k: &str) -> String {
        self.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// 抓取出错（Python 版是 httpx 的几种异常和 ValueError）
pub enum FetchErr {
    /// HTTP 报错（raise_for_status）
    Status(Resp),
    Timeout,
    Request(String),
    /// 内网地址、跳转太多、太大
    Value(String),
}

impl FetchErr {
    pub fn text(&self) -> String {
        match self {
            FetchErr::Status(r) => format!("HTTP {} {}", r.status, r.reason).trim().to_string(),
            FetchErr::Timeout => "timed out".into(),
            FetchErr::Request(s) | FetchErr::Value(s) => s.clone(),
        }
    }
}

static FAKE_IP: (u32, u32) = (0xC612_0000, 0xFFFE_0000); // 198.18.0.0/15：代理 TUN 模式的假地址，不算内网

fn v4_private(a: std::net::Ipv4Addr) -> bool {
    let n = u32::from(a);
    if n & FAKE_IP.1 == FAKE_IP.0 {
        return false;
    }
    let o = a.octets();
    a.is_private() || a.is_loopback() || a.is_link_local() || a.is_broadcast() || a.is_documentation() || a.is_unspecified()
        || a.is_multicast() || o[0] == 0 || (o[0] == 100 && (o[1] & 0xC0) == 64) || (o[0] == 192 && o[1] == 0 && o[2] == 0) || o[0] >= 240
}

/// 本机、内网、链路本地、100.64/10、保留和组播地址
pub fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => v4_private(a),
        IpAddr::V6(a) => {
            if let Some(m) = a.to_ipv4_mapped() {
                return v4_private(m);
            }
            let s = a.segments();
            a.is_loopback() || a.is_unspecified() || a.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8) || (s[0] & 0xe000) != 0x2000
        }
    }
}

pub async fn private_host(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or("").trim_matches(['[', ']']).to_string();
    if let Ok(ip) = host.parse::<IpAddr>() {
        return private_ip(ip);
    }
    let port = u.port_or_known_default().unwrap_or(80);
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => addrs.into_iter().any(|a| private_ip(a.ip())),
        Err(_) => false,
    }
}

/// 能不能抓这个地址：不能的话返回原因（和 tools.blocked 一样）
pub async fn blocked(url: &str) -> Option<String> {
    let ok = reqwest::Url::parse(url).ok().filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()));
    let Some(u) = ok else { return Some("只能抓取 http / https 网址".into()) };
    if config::get_bool("tools.fetch_private", false) {
        return None;
    }
    if private_host(url).await {
        return Some(format!(
            "没抓取：{} 是本机或内网地址。要抓内网页面，在 config.yaml 里设 tools.fetch_private: true",
            u.host_str().unwrap_or("").trim_matches(['[', ']'])
        ));
    }
    None
}

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).user_agent(UA).build().expect("http client")
});

/// GET 一个公网地址。跳转自己跟，每一跳都检查
pub async fn get(url: &str, timeout: f64, max_bytes: usize, headers: &[(&str, String)], params: &[(&str, String)]) -> Result<Resp, FetchErr> {
    let mut url = url.to_string();
    let mut params: Vec<(String, String)> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    for _ in 0..=MAX_REDIRECTS {
        if let Some(why) = blocked(&url).await {
            return Err(FetchErr::Value(why));
        }
        let mut rb = CLIENT.get(&url).timeout(Duration::from_secs_f64(timeout));
        for (k, v) in headers {
            rb = rb.header(*k, v);
        }
        if !params.is_empty() {
            rb = rb.query(&params);
        }
        params.clear();
        let r = rb.send().await.map_err(|e| if e.is_timeout() { FetchErr::Timeout } else { FetchErr::Request(e.to_string()) })?;
        let status = r.status();
        if status.is_redirection() {
            if let Some(loc) = r.headers().get("location").and_then(|v| v.to_str().ok()) {
                let base = r.url().clone();
                url = base.join(loc).map(|u| u.to_string()).unwrap_or_else(|_| loc.to_string());
                continue;
            }
        }
        let final_url = r.url().to_string();
        let hs = r.headers().clone();
        let mut body = Vec::new();
        let mut s = r;
        loop {
            match tokio::time::timeout(Duration::from_secs_f64(timeout), s.chunk()).await {
                Err(_) => return Err(FetchErr::Timeout),
                Ok(Err(e)) => return Err(if e.is_timeout() { FetchErr::Timeout } else { FetchErr::Request(e.to_string()) }),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(c))) => {
                    body.extend_from_slice(&c);
                    if body.len() > max_bytes {
                        return Err(FetchErr::Value(format!("内容太大（超过 {} MB）", max_bytes / 1_000_000)));
                    }
                }
            }
        }
        let resp = Resp { status: status.as_u16(), reason: status.canonical_reason().unwrap_or("").to_string(), headers: hs, body, url: final_url };
        if status.is_client_error() || status.is_server_error() {
            return Err(FetchErr::Status(resp));
        }
        return Ok(resp);
    }
    Err(FetchErr::Value(format!("跳转超过 {MAX_REDIRECTS} 次")))
}

static SCRIPT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| fancy_regex::Regex::new(r"(?i)<(script|style|noscript|svg|template)[\s\S]*?</\1>").unwrap());
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<!--[\s\S]*?-->").unwrap());
static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?(p|div|br|li|tr|h[1-6]|section|article|header|footer|ul|ol|table|blockquote|pre)\b[^>]*>").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\r\x0c\x0b\u{a0}\u{3000}]+").unwrap());

pub fn unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// 网页 → 文字：去掉脚本、样式和标签，块级元素换行，保留段落
pub fn text_of(page: &str) -> String {
    let page = SCRIPT.replace_all(page, " ");
    let page = COMMENT.replace_all(&page, " ");
    let page = BLOCK.replace_all(&page, "\n");
    let page = unescape(&TAG.replace_all(&page, " "));
    page.split('\n').map(|ln| SPACES.replace_all(ln, " ").trim().to_string()).filter(|ln| !ln.is_empty()).collect::<Vec<_>>().join("\n")
}

pub fn short(s: &str, n: usize) -> String {
    let s = s.trim();
    if py::char_len(s) <= n { s.to_string() } else { format!("{}…", py::head(s, n).trim_end()) }
}

static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

pub fn oneline(s: &str, n: usize) -> String {
    short(&WS.replace_all(s, " "), n)
}

/// 字节 → 文字：按给的编码解（gb2312 / gbk 按 gb18030），认不出的按 UTF-8
pub fn decode(raw: &[u8], enc: &str) -> String {
    let e = enc.trim().to_lowercase();
    let e = if ["gb2312", "gbk", "x-gbk"].contains(&e.as_str()) { "gb18030".to_string() } else { e };
    match encoding_rs::Encoding::for_label(e.as_bytes()) {
        Some(enc) => enc.decode_without_bom_handling(raw).0.into_owned(),
        None => String::from_utf8_lossy(raw).into_owned(),
    }
}
