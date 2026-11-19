//! 订阅源有新文章：RSS 2.0 / RSS 1.0 / Atom / JSON Feed。对应 osa/sources/feeds.py（说明见那里）。

use super::util::{self, FetchErr, oneline, short, text_of};
use crate::py;
use crate::registry::Source;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use regex::Regex;
use roxmltree::{Document, Node};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const MAX_SEEN: usize = 500;
pub const MAX_NEW: usize = 20;
pub const MAX_TEXT: usize = 600;
pub const MAX_BYTES: usize = 8_000_000;
const ACCEPT: &str = "application/rss+xml, application/atom+xml, application/feed+json, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.8";
const RSSHUB: &str = "https://docs.rsshub.app";

async fn fetch_raw(url: &str) -> Result<util::Resp, String> {
    util::get(url, 20.0, MAX_BYTES, &[("Accept", ACCEPT.into())], &[]).await.map_err(|e| match e {
        FetchErr::Status(r) => {
            let why = match r.status {
                401 => "要登录",
                403 => "对方拒绝了，可能不让程序访问",
                404 => "地址不存在",
                410 => "订阅源已经停了",
                429 => "请求太频繁，被对方限流了",
                _ => "",
            };
            format!("订阅源打不开：HTTP {}{}", r.status, if why.is_empty() { String::new() } else { format!("（{why}）") })
        }
        FetchErr::Timeout => "订阅源打不开：超时了".into(),
        FetchErr::Request(s) => format!("订阅源打不开：{}", if s.is_empty() { "RequestError".into() } else { oneline(&s, 150) }),
        FetchErr::Value(s) => s,
    })
}

/// 抓订阅源 → {"title", "items"}。抓到的是网页的话，找页面里写的订阅源地址再抓一次
pub async fn fetch(url: &str) -> Result<Value, String> {
    let r = fetch_raw(url).await?;
    let ct = r.header("content-type");
    let mut feed = parse(&r.body, &ct, &r.url)?;
    if feed.is_none() {
        let found = discover(&decode(&r.body, &ct), &r.url);
        if !found.is_empty() && found != r.url {
            let r2 = fetch_raw(&found).await?;
            feed = parse(&r2.body, &r2.header("content-type"), &r2.url)?;
        }
    }
    feed.ok_or_else(|| format!("这个网址是普通网页，不是订阅源（RSS / Atom）。没有订阅源的网站可以试试用 RSSHub 转一个：{RSSHUB}"))
}

static LINK_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<link\b[^>]*>").unwrap());
static ATTR: LazyLock<fancy_regex::Regex> = LazyLock::new(|| fancy_regex::Regex::new(r#"(?s)([\w:-]+)\s*=\s*(["'])(.*?)\2"#).unwrap());

fn join(base: &str, href: &str) -> String {
    reqwest::Url::parse(base).and_then(|b| b.join(href)).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string())
}

pub fn discover(page: &str, base: &str) -> String {
    let head: String = page.chars().take(300_000).collect();
    let mut best = String::new();
    for tag in LINK_TAG.find_iter(&head) {
        let mut a = std::collections::HashMap::new();
        for c in ATTR.captures_iter(tag.as_str()).filter_map(|c| c.ok()) {
            a.insert(c[1].to_lowercase(), util::unescape(&c[3]));
        }
        let typ = a.get("type").cloned().unwrap_or_default().to_lowercase();
        let rel = a.get("rel").cloned().unwrap_or_default().to_lowercase();
        let Some(href) = a.get("href").filter(|h| !h.is_empty()) else { continue };
        if !rel.split_whitespace().any(|x| x == "alternate") {
            continue;
        }
        if ["application/rss+xml", "application/atom+xml", "application/feed+json", "application/json"].contains(&typ.as_str()) {
            let url = join(base, href.trim());
            if typ == "application/rss+xml" {
                return url;
            }
            if best.is_empty() {
                best = url;
            }
        }
    }
    best
}

static ENC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\s*<\?xml[^>]*encoding\s*=\s*["']([\w.:-]+)"#).unwrap());
static CHARSET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)charset\s*=\s*["']?([\w.:-]+)"#).unwrap());
static START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<\?xml|<rss\b|<feed\b|<rdf:RDF\b|<RDF\b").unwrap());
static HTML: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(?:<!--[\s\S]*?-->\s*)*<(?:!doctype\s+html|html)\b").unwrap());
static DECL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*<\?xml[^>]*\?>").unwrap());
static DOCTYPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<!DOCTYPE[^>\[]*(?:\[[\s\S]*?\])?\s*>").unwrap());
static CTRL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f]").unwrap());
static AMP: LazyLock<fancy_regex::Regex> = LazyLock::new(|| fancy_regex::Regex::new(r"&(?!(?:amp|lt|gt|quot|apos|#\d+|#x[0-9a-fA-F]+);)(\w*;?)").unwrap());
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?(?:p|div|br|a|img|span|strong|em|ul|li)\b").unwrap());

fn decode(raw: &[u8], ctype: &str) -> String {
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    let head = String::from_utf8_lossy(&raw[..raw.len().min(200)]).into_owned();
    let enc = ENC.captures(&head).map(|c| c[1].to_string()).or_else(|| CHARSET.captures(ctype).map(|c| c[1].to_string())).unwrap_or_else(|| "utf-8".into());
    util::decode(raw, &enc)
}

fn entity(name: &str) -> String {
    if let Some(n) = name.strip_suffix(';') {
        let dec = util::unescape(&format!("&{n};"));
        if dec != format!("&{n};") && !n.is_empty() {
            return dec.chars().map(|c| format!("&#{};", c as u32)).collect();
        }
    }
    format!("&amp;{name}")
}

/// 修 XML（去掉声明和 DOCTYPE；解析不了的把实体和控制字符修一修）→ 可以交给 roxmltree 的文字
fn xml_text(text: &str) -> Result<String, String> {
    let t = DECL.replace(text, "");
    let t = DOCTYPE.replace(&t, "").into_owned();
    if Document::parse(&t).is_ok() {
        return Ok(t);
    }
    let t2 = CTRL.replace_all(&t, "").into_owned();
    let mut out = String::new();
    let mut last = 0;
    for m in AMP.captures_iter(&t2).filter_map(|c| c.ok()) {
        let all = m.get(0).unwrap();
        out.push_str(&t2[last..all.start()]);
        out.push_str(&entity(&m[1]));
        last = all.end();
    }
    out.push_str(&t2[last..]);
    match Document::parse(&out) {
        Ok(_) => Ok(out),
        Err(e) => Err(format!("订阅源的 XML 坏了，解析不了（{e}）")),
    }
}

fn ln(n: &Node) -> String {
    n.tag_name().name().to_string()
}

fn node_text(n: Option<Node>) -> String {
    match n {
        Some(n) => n.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect::<String>().trim().to_string(),
        None => String::new(),
    }
}

fn child<'a, 'i>(el: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    el.children().filter(|c| c.is_element()).find(|c| ln(c) == name)
}

fn first(el: Node, names: &[&str]) -> String {
    for deep in [false, true] {
        for n in names {
            let it: Box<dyn Iterator<Item = Node>> = if deep { Box::new(el.descendants().skip(1)) } else { Box::new(el.children()) };
            for c in it.filter(|c| c.is_element()) {
                if ln(&c) == *n {
                    let t = node_text(Some(c));
                    if !t.is_empty() {
                        return t;
                    }
                }
            }
        }
    }
    String::new()
}

fn when(s: &str) -> (Option<f64>, String) {
    let s = s.trim();
    if s.is_empty() {
        return (None, String::new());
    }
    let show = |dt: DateTime<Local>| (Some(dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9), dt.format("%Y-%m-%d %H:%M").to_string());
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return show(dt.with_timezone(&Local));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return show(dt.with_timezone(&Local));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(dt) = DateTime::parse_from_str(s, f) {
            return show(dt.with_timezone(&Local));
        }
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            if let Some(dt) = Local.from_local_datetime(&n).earliest() {
                return (Some(dt.timestamp() as f64), n.format("%Y-%m-%d %H:%M").to_string());
            }
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0).and_then(|n| Local.from_local_datetime(&n).earliest()) {
            return (Some(dt.timestamp() as f64), dt.format("%Y-%m-%d %H:%M").to_string());
        }
    }
    (None, oneline(s, 40))
}

fn plain(raw: &str) -> String {
    let t = text_of(raw);
    if TAGS.is_match(&t) { text_of(&t) } else { t }
}

fn duration(v: &str) -> String {
    let s = v.trim();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return py::head(s, 20);
    }
    let n: u64 = s.parse().unwrap_or(0);
    if n >= 3600 { format!("{}:{:02}:{:02}", n / 3600, n % 3600 / 60, n % 60) } else { format!("{}:{:02}", n / 60, n % 60) }
}

#[allow(clippy::too_many_arguments)]
fn item(id: &str, title: &str, url: &str, w: &str, author: &str, summary: &str, full: &str, media: &str, media_type: &str, dur: &str) -> Value {
    let (ts, shown) = when(w);
    let (s, f) = (plain(summary), plain(full));
    let body = if py::char_len(&s) >= 150 || py::char_len(&s) >= py::char_len(&f) { s } else { f };
    let id = if !id.is_empty() { id.to_string() } else if !url.is_empty() { url.to_string() } else { format!("{title}|{w}") };
    use sha1::Digest;
    let key: String = sha1::Sha1::digest(id.trim().as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string();
    json!({"key": key, "title": oneline(&plain(title), 200), "url": url, "ts": ts, "when": shown, "author": oneline(author, 60),
           "body": body, "media": media, "media_type": media_type, "duration": duration(dur)})
}

fn link(el: Node, base: &str) -> String {
    let mut url = String::new();
    for c in el.children().filter(|c| c.is_element() && ln(c) == "link") {
        let href = c.attribute("href").unwrap_or("").trim().to_string();
        if !href.is_empty() {
            if c.attribute("rel").unwrap_or("alternate") == "alternate" {
                url = href;
                break;
            }
        } else {
            let t = node_text(Some(c));
            if !t.is_empty() {
                url = t;
                break;
            }
        }
    }
    if url.is_empty() {
        if let Some(g) = child(el, "guid") {
            let t = node_text(Some(g));
            if g.attribute("isPermaLink").unwrap_or("true") != "false" && t.starts_with("http") {
                url = t;
            }
        }
    }
    if url.is_empty() {
        let i = node_text(child(el, "id"));
        if i.starts_with("http") {
            url = i;
        }
    }
    if url.is_empty() { url } else { join(base, &url) }
}

fn author(el: Node) -> String {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\S+@\S+\s*\((.+)\)$").unwrap());
    for c in el.children().filter(|c| c.is_element()) {
        let n = ln(&c);
        if n == "author" || n == "creator" {
            let name = c.children().filter(|x| x.is_element() && ln(x) == "name").map(|x| node_text(Some(x))).find(|t| !t.is_empty()).unwrap_or_else(|| node_text(Some(c)));
            return R.captures(&name).map(|m| m[1].to_string()).unwrap_or(name);
        }
    }
    String::new()
}

fn media(el: Node) -> (String, String) {
    for c in el.children().filter(|c| c.is_element()) {
        let n = ln(&c);
        if n == "enclosure" && c.attribute("url").is_some_and(|u| !u.is_empty()) {
            return (c.attribute("url").unwrap_or("").into(), c.attribute("type").unwrap_or("").into());
        }
        if n == "link" && c.attribute("rel") == Some("enclosure") && c.attribute("href").is_some_and(|u| !u.is_empty()) {
            return (c.attribute("href").unwrap_or("").into(), c.attribute("type").unwrap_or("").into());
        }
    }
    (String::new(), String::new())
}

fn xml_item(el: Node, base: &str) -> Value {
    let (m, mt) = media(el);
    item(&first(el, &["guid", "id"]), &first(el, &["title"]), &link(el, base),
         &first(el, &["pubDate", "published", "date", "issued", "updated", "modified"]), &author(el),
         &first(el, &["description", "summary"]), &first(el, &["encoded", "content"]), &m, &mt, &first(el, &["duration"]))
}

fn sv(v: &Value) -> String {
    if py::truthy(v) { py::str_of(v) } else { String::new() }
}

fn json_feed(text: &str, base: &str) -> Result<Value, String> {
    let d: Value = serde_json::from_str(text).map_err(|_| "订阅源的内容不是合法的 JSON".to_string())?;
    if !d.is_object() || !d["items"].is_array() {
        return Err("这个 JSON 不是 JSON Feed（没有 items）".into());
    }
    let mut items = vec![];
    for it in d["items"].as_array().unwrap() {
        if !it.is_object() {
            continue;
        }
        let authors: Vec<Value> = if it["authors"].is_array() { it["authors"].as_array().cloned().unwrap_or_default() } else { vec![it["author"].clone()] };
        let author = authors.iter().filter(|a| a.is_object() && py::truthy(&a["name"])).map(|a| py::str_of(&a["name"])).collect::<Vec<_>>().join("、");
        let att = it["attachments"].as_array().and_then(|a| a.iter().find(|x| x.is_object() && py::truthy(&x["url"])).cloned()).unwrap_or(json!({}));
        let url = if py::truthy(&it["url"]) { sv(&it["url"]) } else { sv(&it["external_url"]) };
        let date = if py::truthy(&it["date_published"]) { sv(&it["date_published"]) } else { sv(&it["date_modified"]) };
        let full = if py::truthy(&it["content_html"]) { sv(&it["content_html"]) } else { sv(&it["content_text"]) };
        items.push(item(&sv(&it["id"]), &sv(&it["title"]), &if url.is_empty() { url.clone() } else { join(base, &url) }, &date, &author,
                        &sv(&it["summary"]), &full, &sv(&att["url"]), &sv(&att["mime_type"]), &sv(&att["duration_in_seconds"])));
    }
    Ok(json!({"title": oneline(&sv(&d["title"]), 80), "items": items}))
}

/// 订阅源原文 → {"title", "items"}。看着是网页返回 None，格式坏得修不好报错
pub fn parse(raw: &[u8], ctype: &str, base: &str) -> Result<Option<Value>, String> {
    let text = decode(raw, ctype);
    let text = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']).to_string();
    if text.starts_with('{') || text.starts_with('[') {
        return json_feed(&text, base).map(Some);
    }
    if HTML.is_match(&text) {
        return Ok(None);
    }
    let m = START.find(&text);
    if m.is_none() && !text.contains('<') {
        return Err("这个网址的内容不是 XML 也不是 JSON，不像订阅源".into());
    }
    let src = match m {
        Some(m) => text[m.start()..].to_string(),
        None => text[text.find('<').unwrap_or(0)..].to_string(),
    };
    let fixed = match xml_text(&src) {
        Ok(t) => t,
        Err(e) => {
            static HTMLTAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<html\b").unwrap());
            if HTMLTAG.is_match(&py::head(&src, 5000)) {
                return Ok(None);
            }
            return Err(e);
        }
    };
    let doc = Document::parse(&fixed).map_err(|e| format!("订阅源的 XML 坏了，解析不了（{e}）"))?;
    let root = doc.root_element();
    let kind = ln(&root).to_lowercase();
    if kind == "html" {
        return Ok(None);
    }
    let chan = child(root, "channel");
    let parents: Vec<Node> = match chan {
        Some(c) => vec![c, root],
        None => vec![root],
    };
    let items: Vec<Value> = parents
        .iter()
        .flat_map(|p| p.children().filter(|el| el.is_element() && (ln(el) == "item" || ln(el) == "entry")).map(|el| xml_item(el, base)).collect::<Vec<_>>())
        .collect();
    if items.is_empty() && !["rss", "rdf", "feed"].contains(&kind.as_str()) {
        return Err("这个网址的内容不像订阅源（没有 RSS / Atom 的条目）".into());
    }
    let title = oneline(&plain(&node_text(child(chan.unwrap_or(root), "title"))), 80);
    Ok(Some(json!({"title": title, "items": items})))
}

fn event(it: &Value, feed_title: &str) -> Value {
    let s = |k: &str| it[k].as_str().unwrap_or("").to_string();
    let meta: Vec<String> = [
        if feed_title.is_empty() { String::new() } else { format!("来自：{feed_title}") },
        if s("author").is_empty() { String::new() } else { format!("作者：{}", s("author")) },
        if s("when").is_empty() { String::new() } else { format!("发布：{}", s("when")) },
    ]
    .into_iter()
    .filter(|x| !x.is_empty())
    .collect();
    let mut lines = vec![meta.join("　"), short(&s("body"), MAX_TEXT)];
    if !s("media").is_empty() {
        let mt = s("media_type");
        let kind = if mt.starts_with("audio") { "音频" } else if mt.starts_with("video") { "视频" } else { "附件" };
        lines.push(format!("{kind}：{}{}", s("media"), if s("duration").is_empty() { String::new() } else { format!("（时长 {}）", s("duration")) }));
    }
    let title = if !s("title").is_empty() { s("title") } else { let b = oneline(&s("body"), 80); if b.is_empty() { "（没有标题）".into() } else { b } };
    json!({"id": it["key"], "title": title, "url": it["url"], "text": lines.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("\n")})
}

async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let url = py::str_of(&params["url"]);
    let feed = fetch(&url).await?;
    let mut items = feed["items"].as_array().cloned().unwrap_or_default();
    if !items.is_empty() && items.iter().all(|it| it["ts"].is_number()) {
        items.sort_by(|a, b| b["ts"].as_f64().unwrap_or(0.0).partial_cmp(&a["ts"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    }
    let ftitle = feed["title"].as_str().unwrap_or("").to_string();
    let same = state.is_object() && state["url"].as_str() == Some(url.as_str());
    let seen: Vec<String> = if same { state["seen"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default() } else { vec![] };
    let mut all: Vec<String> = vec![];
    for k in items.iter().map(|it| py::str_of(&it["key"])).chain(seen.iter().cloned()) {
        if !all.contains(&k) {
            all.push(k);
        }
    }
    all.truncate(MAX_SEEN);
    let new_state = json!({"url": url, "seen": all});
    if state.is_null() {
        return Ok((items.first().map(|i| vec![event(i, &ftitle)]).unwrap_or_default(), new_state));
    }
    if !same {
        return Ok((vec![], new_state));
    }
    let mut old: std::collections::HashSet<String> = seen.into_iter().collect();
    let mut fresh = vec![];
    for it in &items {
        let k = py::str_of(&it["key"]);
        if old.insert(k) {
            fresh.push(it.clone());
        }
    }
    let mut events: Vec<Value> = fresh.iter().take(MAX_NEW).map(|it| event(it, &ftitle)).collect();
    if fresh.len() > MAX_NEW {
        let t = format!("{}\n（这次一共有 {} 条新的，只列了最新的 {MAX_NEW} 条）", events[0]["text"].as_str().unwrap_or(""), fresh.len());
        events[0]["text"] = json!(t);
    }
    Ok((events, new_state))
}

fn summary(p: &Value) -> String {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://(www\.)?").unwrap());
    let u = sv(&p["url"]);
    oneline(R.replace(&u, "").trim_end_matches('/'), 60)
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("rss");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.summary = Some(summary);
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rss_and_atom() {
        let rss = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>T &amp; Co</title><item><title>A&nbsp;b</title><link>https://x.com/a</link><guid>1</guid><pubDate>Mon, 09 Nov 2026 09:30:00 +0800</pubDate><description>&lt;p&gt;hello&lt;/p&gt;</description></item></channel></rss>"#;
        let f = parse(rss, "", "https://x.com/feed").unwrap().unwrap();
        assert_eq!(f["title"], "T & Co");
        assert_eq!(f["items"][0]["title"], "A\u{a0}b".replace('\u{a0}', " "));
        assert_eq!(f["items"][0]["body"], "hello");
        let atom = br#"<feed xmlns="http://www.w3.org/2005/Atom"><title>X</title><entry><id>u1</id><title>E</title><link rel="alternate" href="/e"/><updated>2026-11-09T01:00:00Z</updated></entry></feed>"#;
        let f = parse(atom, "", "https://x.com/").unwrap().unwrap();
        assert_eq!(f["items"][0]["url"], "https://x.com/e");
        assert!(parse(b"<!doctype html><html></html>", "", "").unwrap().is_none());
    }
}
