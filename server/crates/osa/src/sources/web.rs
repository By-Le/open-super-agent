//! 网页变了、网站挂了：隔一段时间抓一次网页，和上次比。对应 osa/sources/web.py（说明见那里）。
//! 列出变化用的是 Myers 差分（similar），Python 版是 difflib：大多数时候一样，个别长段落的对齐可能不同。

use super::util::{self, FetchErr, oneline, short, text_of};
use crate::py;
use crate::registry::Source;
use regex::Regex;
use serde_json::{Value, json};
use sha2::Digest;
use std::sync::LazyLock;

pub const MAX_KEEP: usize = 60_000;
pub const MAX_DIFF: usize = 3000;
pub const TIMEOUT: f64 = 20.0;

static META_CHARSET: LazyLock<regex::bytes::Regex> = LazyLock::new(|| regex::bytes::Regex::new(r#"(?i)<meta[^>]+charset\s*=\s*["']?\s*([\w-]+)"#).unwrap());
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<title[^>]*>([\s\S]*?)</title>").unwrap());

/// 响应 → 文字。响应头没写编码的，看 <meta charset>
pub fn page_text(r: &util::Resp) -> String {
    static CS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)charset\s*=\s*["']?([\w.:-]+)"#).unwrap());
    let ct = r.header("content-type");
    let enc = CS.captures(&ct).map(|c| c[1].to_string()).unwrap_or_else(|| {
        META_CHARSET.captures(&r.body[..r.body.len().min(4096)]).map(|m| String::from_utf8_lossy(&m[1]).into_owned()).unwrap_or_else(|| "utf-8".into())
    });
    util::decode(&r.body, &enc)
}

fn neterr(e: &FetchErr) -> String {
    match e {
        FetchErr::Timeout => format!("{} 秒内没响应", TIMEOUT as i64),
        other => {
            let s = oneline(&other.text(), 150);
            if s.to_uppercase().contains("CERTIFICATE") || s.to_uppercase().contains("SSL") {
                format!("HTTPS 证书有问题：{s}")
            } else {
                format!("连不上：{}", if s.is_empty() { "RequestError".into() } else { s })
            }
        }
    }
}

async fn fetch(url: &str) -> Result<util::Resp, String> {
    util::get(url, TIMEOUT, 3_000_000, &[], &[]).await.map_err(|e| match e {
        FetchErr::Status(r) => format!("网页打不开：HTTP {}", r.status),
        FetchErr::Value(s) => s,
        other => format!("网页打不开：{}", neterr(&other)),
    })
}

fn host(url: &str) -> String {
    let h = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| url.to_string());
    h.strip_prefix("www.").map(String::from).unwrap_or(h)
}

fn where_(url: &str) -> String {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://(www\.)?").unwrap());
    oneline(R.replace(url, "").trim_end_matches('/'), 60)
}

fn words(kw: &Value) -> Vec<String> {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[,，|｜]+").unwrap());
    let s = if py::truthy(kw) { py::str_of(kw) } else { String::new() };
    R.split(&s).map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect()
}

fn keep(text: &str, ignore: &[String]) -> String {
    let ig: Vec<String> = ignore.iter().filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect();
    text.split('\n').filter(|ln| !ig.iter().any(|w| ln.to_lowercase().contains(w))).collect::<Vec<_>>().join("\n")
}

fn cap(text: &str) -> String {
    let b = text.as_bytes();
    if b.len() <= MAX_KEEP {
        return text.to_string();
    }
    let b = &b[..MAX_KEEP];
    match b.iter().rposition(|x| *x == b'\n') {
        Some(i) => String::from_utf8_lossy(&b[..i]).into_owned(),
        None => String::from_utf8_lossy(b).trim_end_matches('\u{fffd}').to_string(),
    }
}

/// 两份网页文字的变化：+ 新出现的行，- 没了的行，按页面上的顺序
pub fn diff(old: &str, new: &str, limit: usize) -> String {
    let a: Vec<&str> = old.split('\n').collect();
    let b: Vec<&str> = new.split('\n').collect();
    let mut lines = vec![];
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &a, &b) {
        for (tag, s) in op.iter_changes(&a, &b).map(|c| (c.tag(), c.value())) {
            let p = match tag {
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Equal => continue,
            };
            if !s.trim().is_empty() {
                lines.push(format!("{p} {}", oneline(s, 300)));
            }
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let added = lines.iter().filter(|l| l.starts_with('+')).count();
    let mut out = vec![format!("多了 {added} 行，少了 {} 行（+ 是新出现的，- 是没了的）：", lines.len() - added)];
    let mut used = 0;
    for (i, ln) in lines.iter().enumerate() {
        let n = py::char_len(ln);
        if used + n > limit {
            out.push(format!("……还有 {} 行变化，打开链接看", lines.len() - i));
            break;
        }
        out.push(ln.clone());
        used += n + 1;
    }
    out.join("\n")
}

fn around(text: &str, ws: &[String], n: usize) -> String {
    let low: Vec<String> = ws.iter().map(|w| w.to_lowercase()).collect();
    text.split('\n').filter(|ln| low.iter().any(|w| ln.to_lowercase().contains(w))).map(|ln| oneline(ln, 200)).take(n).collect::<Vec<_>>().join("\n")
}

fn has(text: &str, ws: &[String]) -> Vec<String> {
    let low = text.to_lowercase();
    ws.iter().filter(|w| low.contains(&w.to_lowercase())).cloned().collect()
}

fn quote(ws: &[String]) -> String {
    ws.iter().map(|w| format!("「{w}」")).collect()
}

async fn check_change(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let url = py::str_of(&params["url"]);
    let mode = if py::truthy(&params["mode"]) { py::str_of(&params["mode"]) } else { "change".into() };
    let ws = words(&params["keyword"]);
    let ignore: Vec<String> = params["ignore"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default();
    let raw = page_text(&fetch(&url).await?);
    let text = keep(&text_of(&raw), &ignore);
    if text.trim().is_empty() {
        return Err("网页上没读到文字：可能是用 JavaScript 加载内容的页面，这种现在盯不了；也可能是“忽略”的词把整页都去掉了".into());
    }
    let name = match TITLE.captures(&raw) {
        Some(m) if !m[1].trim().is_empty() => oneline(&util::unescape(&m[1]), 40),
        _ => host(&url),
    };
    let keyj = json!([url, mode, ws, ignore]);
    let key: String = sha1::Sha1::digest(py::dumps_u(&keyj).as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string();
    let hits = has(&text, &ws);
    let kept = cap(&text);
    let hash: String = sha2::Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>()[..32].to_string();
    let new_state = json!({"key": key, "hash": hash, "text": kept, "found": !hits.is_empty()});
    if state.is_null() {
        let mut ev = if mode == "change" {
            json!({"title": format!("网页变了：{name}"), "text": format!("（测试用：这是第一次看到的内容，真的变了的时候这里只列出变了的行）\n{}", short(&text, 1500))})
        } else {
            let a = around(&text, &ws, 5);
            json!({"title": format!("现在网页上{}{}：{name}", if hits.is_empty() { "没有" } else { "有" }, quote(&ws)), "text": if a.is_empty() { short(&text, 1500) } else { a }})
        };
        ev["url"] = json!(url);
        return Ok((vec![ev], new_state));
    }
    if state["key"].as_str() != Some(key.as_str()) {
        return Ok((vec![], new_state));
    }
    let old = state["text"].as_str().unwrap_or("").to_string();
    if mode == "change" {
        if state["hash"].as_str() == Some(hash.as_str()) {
            return Ok((vec![], new_state));
        }
        let mut d = diff(&old, &kept, MAX_DIFF);
        if d.is_empty() {
            d = "页面文字变了，但变化在很靠后的地方（太长没存下来）或者只是空白，打开链接看".into();
        }
        return Ok((vec![json!({"title": format!("网页变了：{name}"), "url": url, "text": d})], new_state));
    }
    let was = py::truthy(&state["found"]);
    if mode == "appear" && !hits.is_empty() && !was {
        let d = diff(&old, &kept, 1500);
        return Ok((vec![json!({"title": format!("网页上出现了{}：{name}", quote(&hits)), "url": url,
            "text": format!("含这个词的行：\n{}{}", around(&text, &hits, 5), if d.is_empty() { String::new() } else { format!("\n\n{d}") })})], new_state));
    }
    if mode == "disappear" && was && hits.is_empty() {
        let d = diff(&old, &kept, 1500);
        return Ok((vec![json!({"title": format!("网页上的{}不见了：{name}", quote(&ws)), "url": url,
            "text": format!("{}\n\n页面开头：\n{}", if d.is_empty() { "页面上找不到这个词了".to_string() } else { d }, short(&text, 600))})], new_state));
    }
    Ok((vec![], new_state))
}

fn validate_change(p: Value) -> Result<Value, String> {
    let m = p["mode"].as_str().unwrap_or("");
    if (m == "appear" || m == "disappear") && words(&p["keyword"]).is_empty() {
        return Err("选了“出现”或“消失”，要填关键词".into());
    }
    Ok(p)
}

fn summary_change(p: &Value) -> String {
    let ws = words(&p["keyword"]);
    let u = where_(p["url"].as_str().unwrap_or(""));
    match p["mode"].as_str() {
        Some("appear") if !ws.is_empty() => format!("{u} +{}", ws.join("/")),
        Some("disappear") if !ws.is_empty() => format!("{u} -{}", ws.join("/")),
        _ => u,
    }
}

// ---------- 网站挂了 ----------
fn secs(ms: i64) -> String {
    if ms < 10_000 { format!("{:.2} 秒", ms as f64 / 1000.0) } else { format!("{} 秒", ms / 1000) }
}

fn ago(seconds: f64) -> String {
    let m = (seconds / 60.0).floor() as i64;
    if m < 1 {
        return "不到 1 分钟".into();
    }
    if m < 60 {
        return format!("{m} 分钟");
    }
    let (h, m) = (m / 60, m % 60);
    if h < 24 {
        return format!("{h} 小时{}", if m > 0 { format!(" {m} 分钟") } else { String::new() });
    }
    let (d, h) = (h / 24, h % 24);
    format!("{d} 天{}", if h > 0 { format!(" {h} 小时") } else { String::new() })
}

fn clock(ts: f64) -> String {
    py::strftime_at(ts, "%m-%d %H:%M")
}

pub async fn probe(url: &str, expect: &str) -> Value {
    let t0 = std::time::Instant::now();
    let ms = || t0.elapsed().as_millis() as i64;
    match util::get(url, TIMEOUT, 10_000_000, &[], &[]).await {
        Err(FetchErr::Status(r)) => json!({"ok": false, "status": r.status, "ms": ms(), "error": format!("HTTP {} {}", r.status, r.reason).trim().to_string()}),
        Err(FetchErr::Value(e)) => json!({"ok": false, "status": 0, "ms": ms(), "error": e}),
        Err(e) => json!({"ok": false, "status": 0, "ms": ms(), "error": neterr(&e), "timeout": matches!(e, FetchErr::Timeout)}),
        Ok(r) => {
            let mut out = json!({"ok": true, "status": r.status, "ms": ms(), "error": ""});
            if !expect.is_empty() && !page_text(&r).to_lowercase().contains(&expect.to_lowercase()) {
                out["ok"] = json!(false);
                out["error"] = json!(format!("页面打得开（HTTP {}），但上面没有「{expect}」", r.status));
            }
            out
        }
    }
}

async fn check_down(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let url = py::str_of(&params["url"]);
    if let Some(why) = util::blocked(&url).await {
        return Err(why);
    }
    let need = py::to_int(&params["fails"]).filter(|n| *n != 0).unwrap_or(2).max(1);
    let expect = if py::truthy(&params["expect"]) { py::str_of(&params["expect"]).trim().to_string() } else { String::new() };
    let p = probe(&url, &expect).await;
    let now = py::now();
    let st = if state.is_object() && state["url"].as_str() == Some(url.as_str()) { state.clone() } else { json!({}) };
    if p["ok"] == true {
        let mut events = vec![];
        let notify = params.get("notify_recover").map(py::truthy).unwrap_or(true);
        if py::truthy(&st["alerted"]) && notify {
            let since = st["since"].as_f64().filter(|s| *s != 0.0).unwrap_or(now);
            let last = st["last"]["error"].as_str().unwrap_or("").to_string();
            events.push(json!({"id": format!("up-{}", now as i64), "title": format!("网站恢复了：{}（打不开了大约 {}）", host(&url), ago(now - since)),
                "url": url, "text": format!("地址：{url}\n现在：HTTP {}，响应 {}\n从 {} 到 {} 打不开，大约 {}{}", py::str_of(&p["status"]), secs(p["ms"].as_i64().unwrap_or(0)),
                    clock(since), clock(now), ago(now - since), if last.is_empty() { String::new() } else { format!("\n当时的情况：{last}") })}));
        }
        return Ok((events, json!({"url": url, "fails": 0, "since": null, "alerted": false, "last": p})));
    }
    let fails = py::to_int(&st["fails"]).unwrap_or(0) + 1;
    let since = st["since"].as_f64().filter(|s| *s != 0.0).unwrap_or(now);
    let alerted = py::truthy(&st["alerted"]);
    let new_state = json!({"url": url, "fails": fails, "since": since, "alerted": alerted || fails >= need, "last": p});
    if alerted || fails < need {
        return Ok((vec![], new_state));
    }
    let took = if p["timeout"] == true { format!("超过 {} 秒", TIMEOUT as i64) } else { secs(p["ms"].as_i64().unwrap_or(0)) };
    let err = p["error"].as_str().unwrap_or("").to_string();
    Ok((vec![json!({"id": format!("down-{}", since as i64), "title": format!("网站打不开了：{}（{}）", host(&url), oneline(&err, 60)), "url": url,
        "text": format!("地址：{url}\n情况：{err}\n响应时间：{took}\n从 {} 开始，连续 {fails} 次检查都失败", clock(since))})], new_state))
}

pub fn sources() -> Vec<Source> {
    let mut c = super::base("web_change");
    c.check = Some(|p, s| Box::pin(check_change(p, s)));
    c.validate = Some(validate_change);
    c.summary = Some(summary_change);
    let mut d = super::base("site_down");
    d.check = Some(|p, s| Box::pin(check_down(p, s)));
    d.summary = Some(|p| where_(p["url"].as_str().unwrap_or("")));
    vec![c, d]
}

#[cfg(test)]
mod tests {
    #[test]
    fn diff_lines() {
        let d = super::diff("a\nb\nc", "a\nB\nc\nd", 3000);
        assert!(d.starts_with("多了 2 行，少了 1 行"));
        assert!(d.contains("- b") && d.contains("+ B") && d.contains("+ d"));
    }
}
