//! 链接卡片：服务器去取网页的标题、摘要、配图、站点名和图标，前端画成书签卡片。对应 osa/unfurl.py（说明见那里）。

use crate::db::{self, RowExt};
use crate::web::{Q, Res, qs};
use crate::{config, py};
use axum::routing::get as route_get;
use futures::StreamExt;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::OnceCell;

pub const KEEP_OK: f64 = 7.0 * 86400.0;
pub const KEEP_FAIL: f64 = 3600.0;
pub const TIMEOUT: u64 = 8;
pub const MAX_BYTES: usize = 512 * 1024;
pub const MAX_REDIRECTS: usize = 5;

static INFLIGHT: LazyLock<Mutex<HashMap<String, Arc<OnceCell<Option<Value>>>>>> = LazyLock::new(Default::default);

pub fn migrate() {
    db::batch("create table if not exists unfurls(url text primary key, data text, ok integer, fetched real)");
}

// ---------- 能不能抓（和 tools.blocked 一样；TODO：tools 迁好后用它的） ----------
fn private_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        x => x,
    };
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            if o[0] == 198 && (o[1] & 0xfe) == 18 {
                return false; // Clash / Surge 的假地址
            }
            a.is_private() || a.is_loopback() || a.is_link_local() || a.is_unspecified() || a.is_broadcast() || a.is_documentation()
                || a.is_multicast() || o[0] == 0 || (o[0] == 100 && (o[1] & 0xc0) == 64) || o[0] >= 240 || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            a.is_loopback() || a.is_unspecified() || a.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || s[0] == 0x2001 && s[1] == 0xdb8
        }
    }
}

async fn private_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let host = host.trim_matches(['[', ']']);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return private_ip(ip);
    }
    let port = url.port_or_known_default().unwrap_or(80);
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.into_iter().any(|a| private_ip(a.ip())),
        Err(_) => false,
    }
}

async fn blocked(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return true };
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(|h| h.is_empty()) {
        return true;
    }
    if config::get_bool("tools.fetch_private", false) {
        return false;
    }
    private_host(&u).await || crate::tools::private_host(url).await
}

// ---------- 解析网页开头 ----------
#[derive(Default)]
struct Head {
    meta: HashMap<String, String>,
    meta_order: Vec<String>,
    title: String,
    icon: String,
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    static ENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&(#[0-9]+|#[xX][0-9a-fA-F]+|[A-Za-z][A-Za-z0-9]*);?").unwrap());
    ENT.replace_all(s, |c: &regex::Captures| {
        let e = &c[1];
        let ch = if let Some(h) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
            u32::from_str_radix(h, 16).ok().and_then(char::from_u32)
        } else if let Some(d) = e.strip_prefix('#') {
            d.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match e {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "middot" => Some('·'),
                "copy" => Some('©'),
                "reg" => Some('®'),
                "laquo" => Some('«'),
                "raquo" => Some('»'),
                "ldquo" => Some('“'),
                "rdquo" => Some('”'),
                "lsquo" => Some('‘'),
                "rsquo" => Some('’'),
                _ => None,
            }
        };
        ch.map(|c| c.to_string()).unwrap_or_else(|| c[0].to_string())
    })
    .into_owned()
}

fn attrs(s: &str) -> HashMap<String, String> {
    static A: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([^\s=/>"']+)(?:\s*=\s*("[^"]*"|'[^']*'|[^\s>]+))?"#).unwrap());
    let mut out = HashMap::new();
    for c in A.captures_iter(s) {
        let k = c[1].to_lowercase();
        let v = c.get(2).map(|v| {
            let v = v.as_str();
            let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
            unescape(v)
        });
        out.entry(k).or_insert(v.unwrap_or_default());
    }
    out
}

fn scan(text: &str) -> Head {
    let mut h = Head::default();
    let mut i = 0;
    let b = text.as_bytes();
    let mut in_title = false;
    let mut title_done = false;
    while i < b.len() {
        let Some(lt) = text[i..].find('<').map(|x| x + i) else {
            if in_title {
                h.title.push_str(&unescape(&text[i..]));
            }
            break;
        };
        if in_title {
            h.title.push_str(&unescape(&text[i..lt]));
        }
        let rest = &text[lt..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map(|x| lt + x + 3).unwrap_or(b.len());
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let inner = &rest[1..gt];
        i = lt + gt + 1;
        if let Some(end) = inner.strip_prefix('/') {
            if end.trim().eq_ignore_ascii_case("title") {
                in_title = false;
                title_done = !h.title.is_empty() || title_done;
            }
            continue;
        }
        if inner.starts_with('!') || inner.starts_with('?') {
            continue;
        }
        let name_end = inner.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(inner.len());
        let tag = inner[..name_end].to_lowercase();
        let a = attrs(&inner[name_end..]);
        match tag.as_str() {
            "meta" => {
                let key = a.get("property").filter(|s| !s.is_empty()).or(a.get("name")).cloned().unwrap_or_default().to_lowercase();
                if let Some(c) = a.get("content").filter(|c| !c.is_empty()) {
                    if !key.is_empty() && !h.meta.contains_key(&key) {
                        h.meta.insert(key.clone(), c.trim().to_string());
                        h.meta_order.push(key);
                    }
                }
            }
            "title" if h.title.is_empty() && !title_done => in_title = true,
            "link" => {
                let rel = a.get("rel").cloned().unwrap_or_default().to_lowercase();
                if rel.split_whitespace().any(|r| r == "icon") && h.icon.is_empty() {
                    if let Some(href) = a.get("href").filter(|s| !s.is_empty()) {
                        h.icon = href.clone();
                    }
                }
            }
            "script" | "style" => {
                let close = format!("</{tag}");
                let low = text[i..].to_ascii_lowercase();
                i = low.find(&close).map(|x| i + x).unwrap_or(b.len());
            }
            "body" => break,
            _ => {}
        }
    }
    h
}

fn charset(head: &[u8], header: Option<&str>) -> String {
    if let Some(h) = header.filter(|h| !h.is_empty()) {
        return h.to_string();
    }
    static RE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| regex::bytes::Regex::new(r#"(?i)<meta[^>]+charset=["']?([\w-]+)"#).unwrap());
    let h = &head[..head.len().min(4096)];
    RE.captures(h).map(|c| String::from_utf8_lossy(&c[1]).into_owned()).unwrap_or_else(|| "utf-8".into())
}

fn abs(base: &str, u: Option<&str>) -> String {
    let u = u.unwrap_or("").trim();
    if u.is_empty() || u.starts_with("data:") {
        return String::new();
    }
    let u = if u.starts_with("//") { format!("https:{u}") } else { u.to_string() };
    match reqwest::Url::parse(base).and_then(|b| b.join(&u)) {
        Ok(x) if x.scheme() == "http" || x.scheme() == "https" => x.to_string(),
        _ => String::new(),
    }
}

fn clean(s: &str, n: usize) -> String {
    py::head(s.split_whitespace().collect::<Vec<_>>().join(" ").trim(), n)
}

/// 从网页开头取卡片信息；连标题都没有返回 None
pub fn parse(html: &[u8], url: &str, encoding: Option<&str>) -> Option<Value> {
    let cs = charset(html, encoding);
    let enc = encoding_rs::Encoding::for_label(cs.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = enc.decode(html);
    let p = scan(&text);
    let m = &p.meta;
    let g = |k: &str| m.get(k).map(|s| s.as_str()).filter(|s| !s.is_empty());
    let title = clean(g("og:title").or(g("twitter:title")).unwrap_or(&p.title), 200);
    if title.is_empty() {
        return None;
    }
    let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    let scheme = url.split("://").next().unwrap_or("https");
    let icon = abs(url, Some(&p.icon));
    let site = clean(g("og:site_name").unwrap_or(""), 60);
    let site = if site.is_empty() { host.strip_prefix("www.").unwrap_or(&host).to_string() } else { site };
    let icon = if !icon.is_empty() { icon } else if !host.is_empty() { format!("{scheme}://{host}/favicon.ico") } else { String::new() };
    let description = clean(g("og:description").or(g("twitter:description")).or(g("description")).unwrap_or(""), 300);
    let image = abs(url, g("og:image:secure_url").or(g("og:image")).or(g("twitter:image")).or(g("twitter:image:src")));
    Some(json!({"url": url, "title": title, "description": description, "image": image, "site": site, "icon": icon}))
}

static YOUTUBE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://((www|m|music)\.)?(youtube\.com|youtu\.be)/").unwrap());
static EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^https?://((www|m|mobile|music)\.)?(bilibili\.com/video/BV|youtube\.com/(watch\?|shorts/|live/)|youtu\.be/|(x|twitter)\.com/[^/?#]+/status(es)?/\d)").unwrap()
});
const KEEP_QS: [&str; 3] = ["v", "t", "p"];

fn landing(url: &str) -> Value {
    let mut u = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return json!({"url": url, "title": "", "description": "", "image": "", "site": "", "icon": "", "embed": url}),
    };
    static TWEET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\.)(x|twitter)\.com$").unwrap());
    let tweet = u.host_str().is_some_and(|h| TWEET.is_match(h));
    let keep: Vec<(String, String)> = if tweet { vec![] } else { u.query_pairs().filter(|(k, _)| KEEP_QS.contains(&k.as_ref())).map(|(k, v)| (k.into_owned(), v.into_owned())).collect() };
    if keep.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(keep);
    }
    u.set_fragment(None);
    let s = u.to_string();
    json!({"url": s, "title": "", "description": "", "image": "", "site": "", "icon": "", "embed": s})
}

fn client() -> reqwest::Client {
    static C: LazyLock<reqwest::Client> = LazyLock::new(|| {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("user-agent", "Mozilla/5.0 (compatible; OpenSuperAgent/1.0; link preview)".parse().unwrap());
        h.insert("accept", "text/html,application/xhtml+xml".parse().unwrap());
        h.insert("accept-language", "zh-CN,zh;q=0.9,en;q=0.8".parse().unwrap());
        reqwest::Client::builder().default_headers(h).redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(TIMEOUT)).build().unwrap()
    });
    C.clone()
}

async fn oembed(url: &str) -> Option<Value> {
    let r = client().get("https://www.youtube.com/oembed").query(&[("url", url), ("format", "json")]).send().await.ok()?;
    if r.status().as_u16() >= 400 {
        return None;
    }
    let b = r.bytes().await.ok()?;
    if b.len() > MAX_BYTES {
        return None;
    }
    let d: Value = serde_json::from_slice(&b).ok()?;
    let title = clean(&if d["title"].is_null() { String::new() } else { py::str_of(&d["title"]) }, 200);
    if title.is_empty() {
        return None;
    }
    Some(json!({"url": url, "title": title, "description": py::head(&if d["author_name"].is_null() { String::new() } else { py::str_of(&d["author_name"]) }, 300),
                "image": abs(url, d["thumbnail_url"].as_str()), "site": "YouTube", "icon": "https://www.youtube.com/favicon.ico"}))
}

async fn fetch(url: &str) -> Option<Value> {
    if YOUTUBE.is_match(url) {
        return oembed(url).await;
    }
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        if blocked(&url).await {
            return None;
        }
        let r = client().get(&url).send().await.ok()?;
        if r.status().is_redirection() {
            if let Some(loc) = r.headers().get("location").and_then(|v| v.to_str().ok()) {
                url = reqwest::Url::parse(&url).ok()?.join(loc).ok()?.to_string();
                if EMBED.is_match(&url) {
                    return Some(landing(&url));
                }
                continue;
            }
        }
        let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("html").to_lowercase();
        if r.status().as_u16() >= 400 || !ct.contains("html") {
            return None;
        }
        let enc = ct.split(';').filter_map(|p| p.trim().strip_prefix("charset=")).next().map(|s| s.trim_matches('"').to_string());
        let final_url = r.url().to_string();
        let mut s = r.bytes_stream();
        let mut buf: Vec<u8> = vec![];
        while let Some(Ok(chunk)) = s.next().await {
            buf.extend_from_slice(&chunk);
            let from = buf.len().saturating_sub(chunk.len() + 8);
            if buf.len() >= MAX_BYTES || buf[from..].to_ascii_lowercase().windows(5).any(|w| w == b"<body") {
                break;
            }
        }
        buf.truncate(MAX_BYTES);
        return parse(&buf, &final_url, enc.as_deref());
    }
    None
}

fn cached(url: &str) -> Option<Option<Value>> {
    let r = db::row("select data, ok, fetched from unfurls where url=?", [url])?;
    let ok_ = r.i(1) != 0;
    if py::now() - r.f(2) > if ok_ { KEEP_OK } else { KEEP_FAIL } {
        return None;
    }
    Some(if ok_ { Some(r.j(0)) } else { None })
}

/// 一个网址的卡片信息；抓不到、不让抓的返回 None
pub async fn get(url: &str) -> Option<Value> {
    static OK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://[^\s/]+").unwrap());
    let url = url.trim().to_string();
    if !OK.is_match(&url) || url.len() > 2000 {
        return None;
    }
    if let Some(hit) = cached(&url) {
        return hit;
    }
    let cell = INFLIGHT.lock().entry(url.clone()).or_insert_with(|| Arc::new(OnceCell::new())).clone();
    let u2 = url.clone();
    let data = cell
        .get_or_init(|| async move {
            let data = tokio::time::timeout(Duration::from_secs(TIMEOUT + 2), fetch(&u2)).await.ok().flatten();
            let now = py::now();
            db::exec(
                "insert or replace into unfurls values(?,?,?,?)",
                rusqlite::params![u2, py::dumps_u(data.as_ref().unwrap_or(&json!({}))), data.is_some() as i64, now],
            );
            db::exec("delete from unfurls where fetched < ?", [now - KEEP_OK]);
            data
        })
        .await
        .clone();
    INFLIGHT.lock().remove(&url);
    data
}

async fn api_unfurl(q: Q) -> Res {
    Ok(axum::Json(match get(qs(&q, "url").unwrap_or("")).await {
        Some(d) => {
            let mut o = json!({"ok": true});
            for (k, v) in d.as_object().cloned().unwrap_or_default() {
                o[k] = v;
            }
            o
        }
        None => json!({"ok": false}),
    }))
}

pub fn routes() -> axum::Router {
    axum::Router::new().route("/api/unfurl", route_get(api_unfurl))
}
