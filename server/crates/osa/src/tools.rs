//! 内置的网页工具：web_search、fetch_url、search_posts。注册在 builtin.rs。对应 osa/tools.py。
//! 用哪家搜索、哪家读网页按配方来（websearch.rs）；这里管安全检查（内网地址、不用的网站）和直接抓网页。

use crate::registry::{Call, Out};
use crate::web::{Body, Res, ok};
use crate::{config, py, websearch};
use axum::routing::{get, post};
use regex::Regex;
use serde_json::Value;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;

const UA: &str = "open-super-agent/0.1";
pub const MAX_REDIRECTS: usize = 5;

/// 天数参数：1–3650，没给是 None
pub fn days_of(v: &Value) -> Option<i64> {
    if v.is_null() || v.as_str() == Some("") || v.as_str() == Some("0") || v.as_i64() == Some(0) || v.as_f64() == Some(0.0) {
        return None;
    }
    let n = match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }?;
    Some(n.clamp(1, 3650))
}

/// 本机、内网、链路本地、100.64/10、保留和组播地址（Clash 之类 TUN 模式的 198.18/15 假地址不算）
pub fn private_ip(ip: &str) -> bool {
    let Ok(mut a) = ip.split('%').next().unwrap_or("").parse::<IpAddr>() else { return false };
    if let IpAddr::V6(v6) = a {
        if let Some(v4) = v6.to_ipv4_mapped() {
            a = IpAddr::V4(v4);
        }
    }
    match a {
        IpAddr::V4(v) => {
            let o = v.octets();
            if o[0] == 198 && (o[1] & 0xfe) == 18 {
                return false;
            }
            !global_v4(v) || v.is_multicast()
        }
        IpAddr::V6(v) => !global_v6(v) || v.is_multicast(),
    }
}

/// ipaddress 的 is_global（IANA 特殊用途表里不是 globally reachable 的都算不是）
fn global_v4(v: std::net::Ipv4Addr) -> bool {
    let o = v.octets();
    let n = u32::from(v);
    let in_net = |base: u32, bits: u32| (n >> (32 - bits)) == (base >> (32 - bits));
    if v.is_unspecified() || v.is_loopback() || v.is_private() || v.is_link_local() || v.is_broadcast() || v.is_documentation() {
        return false;
    }
    if o[0] == 0 || o[0] >= 240 {
        return false;
    }
    if in_net(0x6440_0000, 10) {
        return false; // 100.64.0.0/10
    }
    if in_net(0xC000_0000, 24) && !matches!(n, 0xC000_0009 | 0xC000_000A) {
        return false; // 192.0.0.0/24
    }
    if in_net(0xC612_0000, 15) {
        return false; // 198.18.0.0/15
    }
    true
}

fn global_v6(v: std::net::Ipv6Addr) -> bool {
    let s = v.segments();
    if v.is_unspecified() || v.is_loopback() {
        return false;
    }
    if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false;
    }
    if s[0] == 0x2001 && s[1] < 0x0200 {
        return false;
    }
    if s[0] == 0x0064 && s[1] == 0xff9b {
        return false;
    }
    if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
        return false;
    }
    true
}

fn is_ip(host: &str) -> bool {
    host.split('%').next().unwrap_or("").parse::<IpAddr>().is_ok()
}

/// 网址的主机是不是本机或内网地址：先解析域名，解析到内网的也算。解析不了的算不是
pub async fn private_host(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or("").trim_matches(['[', ']']).to_string();
    if host.is_empty() {
        return false;
    }
    let ips: Vec<String> = if is_ip(&host) {
        vec![host]
    } else {
        let port = u.port().unwrap_or(if u.scheme() == "https" { 443 } else { 80 });
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(a) => a.map(|x| x.ip().to_string()).collect(),
            Err(_) => return false,
        }
    };
    ips.iter().any(|ip| private_ip(ip))
}

/// fetch_url 能不能抓这个地址：不能的话返回原因
pub async fn blocked(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok();
    let ok_scheme = u.as_ref().is_some_and(|u| (u.scheme() == "http" || u.scheme() == "https") && u.host_str().is_some_and(|h| !h.is_empty()));
    if !ok_scheme {
        return Some("只能抓取 http / https 网址".into());
    }
    if config::get_bool("tools.fetch_private", false) {
        return None;
    }
    if private_host(url).await {
        let h = u.and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string())).unwrap_or_default();
        return Some(format!("没抓取：{h} 是本机或内网地址。要抓内网页面，在 config.yaml 里设 tools.fetch_private: true"));
    }
    None
}

pub async fn web_search(query: &str, days: &Value) -> String {
    websearch::search(query, days_of(days), None).await
}

pub async fn search_posts(platform: &str, query: &str, days: &Value, count: &Value, browser: bool) -> String {
    let count = if count.is_null() || count.as_str() == Some("") { None } else { py::to_int(count).filter(|_| !count.is_array() && !count.is_object()) };
    websearch::search_posts(platform, query, days_of(days), count, None, browser).await
}

pub async fn fetch_url(url: &str, browser: bool) -> String {
    let url = url.trim();
    if let Some(why) = blocked(url).await {
        return why;
    }
    if websearch::blocked_domain(url).is_some() {
        return websearch::refuse_text(url, true);
    }
    websearch::fetch(url, private_host(url).await, None, browser).await
}

/// 自己下载网页、去掉标签（配方里的 direct）。跳转自己跟：每一跳都要检查
pub async fn direct_fetch(url: &str) -> String {
    let c = reqwest::Client::builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default();
    let mut url = url.to_string();
    let mut body = None;
    for _ in 0..=MAX_REDIRECTS {
        if let Some(why) = blocked(&url).await {
            return why;
        }
        if websearch::blocked_domain(&url).is_some() {
            return websearch::refuse_text(&url, true);
        }
        let r = match c.get(&url).header("User-Agent", UA).send().await {
            Ok(r) => r,
            Err(e) => return format!("没抓取：{e}"),
        };
        let loc = r.headers().get("location").and_then(|v| v.to_str().ok()).map(String::from);
        if r.status().is_redirection() && loc.as_deref().is_some_and(|l| !l.is_empty()) {
            let next = r.url().join(loc.as_deref().unwrap()).map(|u| u.to_string()).unwrap_or_default();
            url = next;
            continue;
        }
        body = Some(r.text().await.unwrap_or_default());
        break;
    }
    let Some(text) = body else { return format!("没抓取：跳转超过 {MAX_REDIRECTS} 次") };
    static SCRIPT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| fancy_regex::Regex::new(r"<(script|style)[\s\S]*?</\1>").unwrap());
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    let t = SCRIPT.replace_all(&text, "").into_owned();
    let t = websearch::unescape(&TAG.replace_all(&t, " "));
    py::head(&WS.replace_all(&t, " "), 8000)
}

pub async fn tool_web_search(c: Call) -> Out {
    Out::Text(web_search(&c.arg_str("query"), &c.args["days"]).await)
}

pub async fn tool_search_posts(c: Call) -> Out {
    let a = &c.args;
    Out::Text(search_posts(&crate::builtin::arg_s(a, "platform"), &crate::builtin::arg_s(a, "query"), &a["days"], &a["count"], py::truthy(&c.env["use_pc"])).await)
}

pub async fn tool_fetch_url(c: Call) -> Out {
    Out::Text(fetch_url(&crate::builtin::arg_s(&c.args, "url"), py::truthy(&c.env["use_pc"])).await)
}

// ---------- 接口：设置 → 搜索和读网页 ----------
async fn web_settings() -> Res {
    ok(websearch::public())
}

async fn save_web_settings(Body(d): Body) -> Res {
    ok(websearch::save(&d)?)
}

async fn test_web_service(Body(d): Body) -> Res {
    let s = |k: &str| if py::truthy(&d[k]) { py::str_of(&d[k]) } else { String::new() };
    ok(websearch::test(&s("kind"), &s("name")).await?)
}

/// /api/web、/api/web/test
pub fn routes() -> axum::Router {
    axum::Router::new().route("/api/web", get(web_settings).put(save_web_settings)).route("/api/web/test", post(test_web_service))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private() {
        assert!(private_ip("127.0.0.1"));
        assert!(private_ip("10.1.2.3"));
        assert!(private_ip("169.254.169.254"));
        assert!(private_ip("100.100.1.1"));
        assert!(!private_ip("198.18.0.5"));
        assert!(!private_ip("8.8.8.8"));
        assert!(private_ip("::1"));
        assert!(private_ip("::ffff:192.168.1.1"));
        assert!(private_ip("224.0.0.1"));
    }
}
