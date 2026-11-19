//! Bot 的资料库：它交给用户的东西（页面、图片、文件、链接），按时间倒序。右栏“资料库”标签用。对应 osa/outputs.py。

use crate::db::{self, RowExt};
use crate::web::Res;
use crate::{py, server};
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::routing::get;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

pub const SCAN: i64 = 3000;

static INTERNAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(library|api|files|media|s)(/|$)").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?(```|$)").unwrap());
static MD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"!?\[([^\]\n]{1,300})\]\((https?://(?:[^\s()]|\([^\s()]*\))+)(?:\s+"[^"\n]*")?\)"#).unwrap());
static BARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[A-Za-z0-9.\-]+(?::\d+)?(?:[/?#][^\s<>"'`|\\^{}\x{3000}-\x{303f}\x{ff00}-\x{ffef}]*)?"#).unwrap()
});
static CJK_PUNCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x{3000}-\x{303f}\x{ff00}-\x{ffef}\x{2018}-\x{201f}…]").unwrap());
static RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[任务 #(\d+) 结果\]").unwrap());
const TRAIL: &str = ".,;:!?'\"*_~";

/// 去掉粘在链接后面的标点
pub fn clean_url(u: &str) -> String {
    let mut u = match CJK_PUNCT.find(u) {
        Some(m) => u[..m.start()].to_string(),
        None => u.to_string(),
    };
    while let Some(c) = u.chars().last() {
        let open = match c {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            _ => None,
        };
        if TRAIL.contains(c) {
            u.pop();
        } else if let Some(o) = open.filter(|o| u.matches(c).count() > u.matches(*o).count()) {
            let _ = o;
            u.pop();
        } else {
            break;
        }
    }
    u
}

fn title_of(text: &str) -> String {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[*_`~]+").unwrap());
    py::head(&RE.replace_all(text, "").split_whitespace().collect::<Vec<_>>().join(" "), 160)
}

/// urlsplit 的一部分：(scheme, netloc, hostname, path)
pub fn split(u: &str) -> Option<(String, String, String, String)> {
    let (scheme, rest) = u.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = &rest[..end];
    let after = &rest[end..];
    let path = after.split(['?', '#']).next().unwrap_or("");
    let hostport = netloc.rsplit('@').next().unwrap_or("");
    let host = if let Some(h) = hostport.strip_prefix('[') {
        h.split(']').next()?.to_string()
    } else {
        hostport.split(':').next().unwrap_or("").to_string()
    };
    Some((scheme.to_lowercase(), netloc.to_string(), host.to_lowercase(), path.to_string()))
}

pub fn valid(u: &str) -> bool {
    match split(u) {
        Some((s, _, host, _)) => (s == "http" || s == "https") && (host.trim_matches('.').contains('.') || host == "localhost"),
        None => false,
    }
}

pub fn internal(u: &str, own: &HashSet<String>) -> bool {
    let Some((_, netloc, host, path)) = split(u) else { return false };
    let local = own.contains(&netloc.to_lowercase()) || host == "localhost" || host == "127.0.0.1";
    local && INTERNAL.is_match(if path.is_empty() { "/" } else { &path })
}

/// 正文里的 http(s) 链接，按出现顺序去重：[(网址, 标题)]。代码块里的不算
pub fn links(text: &str) -> Vec<(String, String)> {
    let text = FENCE.replace_all(text, " ").into_owned();
    let mut out: IndexMap<String, String> = IndexMap::new();
    for m in MD.captures_iter(&text) {
        let u = clean_url(&m[2]);
        let mut title = title_of(&m[1]);
        if title == u || title.starts_with("http://") || title.starts_with("https://") {
            title.clear();
        }
        if valid(&u) && out.get(&u).is_none_or(|t| t.is_empty()) {
            out.insert(u, title);
        }
    }
    let text = MD.replace_all(&text, " ").into_owned();
    for m in BARE.find_iter(&text) {
        let u = clean_url(m.as_str());
        if valid(&u) {
            out.entry(u).or_default();
        }
    }
    out.into_iter().collect()
}

fn source(role: &str, content: &str, meta: &Value) -> (bool, Value) {
    if role == "assistant" {
        return (true, Value::Null);
    }
    match meta.get("kind").filter(|k| !k.is_null()) {
        Some(k) if k == "result" => (true, meta.get("task_id").cloned().unwrap_or(Value::Null)),
        Some(k) if k == "image" => (true, Value::Null),
        None => match RESULT.captures(content) {
            Some(c) => (true, json!(c[1].parse::<i64>().unwrap_or(0))),
            None => (false, Value::Null),
        },
        _ => (false, Value::Null),
    }
}

fn merge(a: &Value, b: Value) -> Value {
    py::merge(a, &b)
}

pub fn for_bot(bot_id: i64, limit: usize, own_hosts: &[String]) -> Vec<Value> {
    let own: HashSet<String> = own_hosts.iter().filter(|h| !h.is_empty()).map(|h| h.to_lowercase()).collect();
    let mut items: IndexMap<String, Value> = IndexMap::new();
    fn put(items: &mut IndexMap<String, Value>, key: String, mut item: Value) {
        match items.get_mut(&key) {
            None => {
                item["key"] = json!(key);
                item["count"] = json!(1);
                items.insert(key, item);
            }
            Some(old) => old["count"] = json!(old["count"].as_i64().unwrap_or(0) + 1),
        }
    }
    let rows = db::rows(
        "select id, ts, role, content, meta, chat_id from messages
        where bot_id=? and role in ('assistant', 'task')
          and (meta like '%\"attachments\"%' or content like '%http%')
        order by id desc limit ?",
        rusqlite::params![bot_id, SCAN],
    );
    for r in rows {
        let m = r.j(4);
        let content = r.s(3);
        let (ok, task_id) = source(&r.s(2), &content, &m);
        if !ok {
            continue;
        }
        let src = json!({"ts": r[1], "chat_id": r[5], "msg_id": r[0], "task_id": task_id});
        for a in m["attachments"].as_array().cloned().unwrap_or_default() {
            if !a.is_object() {
                continue;
            }
            if py::truthy(&a["media"]) {
                let media = py::str_of(&a["media"]);
                put(&mut items, format!("image:{media}"), merge(&src, json!({"kind": "image",
                    "title": if py::truthy(&a["name"]) { a["name"].clone() } else { json!("") }, "media": media,
                    "mime": a.get("mime").cloned().unwrap_or(Value::Null), "size": a.get("size").cloned().unwrap_or(Value::Null),
                    "w": a.get("w").cloned().unwrap_or(Value::Null), "h": a.get("h").cloned().unwrap_or(Value::Null),
                    "path": a.get("path").cloned().unwrap_or(Value::Null)})));
            } else if py::truthy(&a["path"]) {
                let path = py::str_of(&a["path"]);
                let mac = a["mac"].as_i64().filter(|_| a["mac"].is_i64());
                let title = if py::truthy(&a["name"]) { py::str_of(&a["name"]) } else { path.rsplit('/').next().unwrap_or("").to_string() };
                put(&mut items, format!("file:{}:{path}", mac.filter(|m| *m != 0).map(|m| m.to_string()).unwrap_or_default()),
                    merge(&src, json!({"kind": "file", "title": title, "path": path, "size": a.get("size").cloned().unwrap_or(Value::Null), "mac": mac})));
            }
        }
        for (u, title) in links(&content) {
            if internal(&u, &own) {
                continue;
            }
            let key = format!("link:{u}");
            if let Some(old) = items.get_mut(&key) {
                old["count"] = json!(old["count"].as_i64().unwrap_or(0) + 1);
                if old["title"].as_str().unwrap_or("").is_empty() && !title.is_empty() {
                    old["title"] = json!(title);
                }
                continue;
            }
            let host = split(&u).map(|x| x.2).unwrap_or_default();
            put(&mut items, key, merge(&src, json!({"kind": "link", "title": title, "url": u, "host": host})));
        }
    }
    for r in db::rows("select id, title, format, share_token, task_id, created, updated from pages where bot_id=?", [bot_id]) {
        put(&mut items, format!("page:{}", r.i(0)), json!({"kind": "page", "ts": r[5], "updated": r[6], "title": r[1], "page_id": r[0],
            "format": r[2], "shared": py::truthy(&r[3]), "task_id": r[4], "chat_id": null, "msg_id": null}));
    }
    let mut out: Vec<Value> = items.into_values().collect();
    out.sort_by(|a, b| b["ts"].as_f64().unwrap_or(0.0).partial_cmp(&a["ts"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit);
    name_links(&mut out);
    out
}

/// 裸链接用网页标题当名字：链接卡片抓成功过的直接拿来用
fn name_links(items: &mut [Value]) {
    let bare: Vec<usize> = (0..items.len()).filter(|i| items[*i]["kind"] == "link" && items[*i]["title"].as_str().unwrap_or("").is_empty()).collect();
    for chunk in bare.chunks(500) {
        let urls: Vec<String> = chunk.iter().map(|i| py::str_of(&items[*i]["url"])).collect();
        let rows = db::rows(&format!("select url, data from unfurls where ok=1 and url in ({})", vec!["?"; urls.len()].join(",")), rusqlite::params_from_iter(urls.iter()));
        let got: std::collections::HashMap<String, String> = rows.iter().map(|r| (r.s(0), r.j(1)["title"].as_str().unwrap_or("").to_string())).collect();
        for i in chunk {
            if let Some(t) = got.get(items[*i]["url"].as_str().unwrap_or("")).filter(|t| !t.is_empty()) {
                items[*i]["title"] = json!(t);
            }
        }
    }
}

async fn api_outputs(Path(bot_id): Path<i64>, h: HeaderMap) -> Res {
    server::bot_or_404(bot_id)?;
    let host = h.get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    Ok(axum::Json(json!(for_bot(bot_id, 500, &[host]))))
}

pub fn routes() -> axum::Router {
    axum::Router::new().route("/api/bots/{bot_id}/outputs", get(api_outputs))
}
