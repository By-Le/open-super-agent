//! 对话里的图片和附件（media 表）。对应 osa/media.py（说明见那里）。

use crate::db::{self, RowExt};
use crate::{config, library, memory, py, t};
use regex::Regex;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

pub const MAX_BYTES: usize = 10 << 20;
pub const MAX_ATTACHMENTS: usize = 10;
pub const KEEP_S: f64 = 86400.0;
pub const MODEL_IMAGES: usize = 4;
pub const MODEL_MIMES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];
pub const MAX_SEND: usize = 5;
pub const MAX_VIDEO_REF: usize = 100 << 20;

static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{24}$").unwrap());

pub fn migrate() {
    db::batch(
        "create table if not exists media(id text primary key, mime text, name text default '', size integer, w integer default 0, h integer default 0,
          data blob, created real)",
    );
}

pub fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if b.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if b.len() >= 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        return Some(if &b[8..12] == b"qt  " { "video/quicktime" } else { "video/mp4" });
    }
    if b.starts_with(b"\x1aE\xdf\xa3") {
        return Some("video/webm");
    }
    None
}

fn be(b: &[u8], a: usize, z: usize) -> Option<u32> {
    b.get(a..z).map(|s| s.iter().fold(0u32, |acc, x| (acc << 8) | *x as u32))
}

fn le(b: &[u8], a: usize, z: usize) -> Option<u32> {
    b.get(a..z).map(|s| s.iter().rev().fold(0u32, |acc, x| (acc << 8) | *x as u32))
}

pub fn dimensions(b: &[u8], mime: &str) -> (u32, u32) {
    let r = (|| -> Option<(u32, u32)> {
        match mime {
            "image/png" => Some((be(b, 16, 20)?, be(b, 20, 24)?)),
            "image/gif" => Some((le(b, 6, 8)?, le(b, 8, 10)?)),
            "image/webp" => match b.get(12..16)? {
                b"VP8 " => Some((le(b, 26, 28)? & 0x3FFF, le(b, 28, 30)? & 0x3FFF)),
                b"VP8L" => {
                    let (b21, b22, b23, b24) = (*b.get(21)? as u32, *b.get(22)? as u32, *b.get(23)? as u32, *b.get(24)? as u32);
                    Some((1 + (((b22 & 0x3F) << 8) | b21), 1 + (((b24 & 0x0F) << 10) | (b23 << 2) | ((b22 & 0xC0) >> 6))))
                }
                b"VP8X" => Some((1 + le(b, 24, 27)?, 1 + le(b, 27, 30)?)),
                _ => None,
            },
            "image/jpeg" => {
                let mut i = 2;
                while i + 9 < b.len() {
                    if b[i] != 0xFF {
                        break;
                    }
                    let marker = b[i + 1];
                    if marker == 0xFF {
                        i += 1;
                        continue;
                    }
                    if [0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF].contains(&marker) {
                        return Some((be(b, i + 7, i + 9)?, be(b, i + 5, i + 7)?));
                    }
                    i += 2 + be(b, i + 2, i + 4)? as usize;
                }
                None
            }
            _ => None,
        }
    })();
    r.unwrap_or((0, 0))
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn clean_name(name: &str, mime: &str) -> String {
    let n = name.replace('\\', "/");
    let n = py::head(basename(&n).trim().replace('\0', "").as_str(), 200);
    if n.is_empty() { format!("image.{}", mime.split('/').nth(1).unwrap_or("png").replace("jpeg", "jpg")) } else { n }
}

pub fn save(data: &[u8], name: &str, limit: usize, video: bool) -> Result<Value, String> {
    if data.is_empty() {
        return Err(t!("没收到图片"));
    }
    if data.len() > limit {
        return Err(t!("图片太大了（最多 {n} MB）", n = limit >> 20));
    }
    let mime = match sniff(data) {
        Some(m) if !(m.starts_with("video/") && !video) => m,
        _ => return Err(t!("只支持 PNG、JPG、WebP、GIF 图片")),
    };
    let mid = py::head(&crate::util::sha256_hex(data), 24);
    let (w, h) = dimensions(data, mime);
    let name = clean_name(name, mime);
    db::exec(
        "insert into media(id, mime, name, size, w, h, data, created) values(?,?,?,?,?,?,?,?)
        on conflict(id) do update set created=excluded.created",
        params![mid, mime, name, data.len() as i64, w, h, data, py::now()],
    );
    Ok(json!({"id": mid, "mime": mime, "name": name, "size": data.len(), "w": w, "h": h}))
}

pub fn get(mid: &str) -> Option<(String, Vec<u8>, String)> {
    if !ID.is_match(mid) {
        return None;
    }
    let c = db::conn();
    c.query_row("select mime, data, name from media where id=?", params![mid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2).unwrap_or_default()))).ok()
}

pub fn info(mid: &Value) -> Option<Value> {
    let mid = mid.as_str().filter(|m| ID.is_match(m))?;
    let r = db::row("select mime, name, size, w, h from media where id=?", params![mid])?;
    Some(json!({"id": mid, "mime": r[0], "name": r[1], "size": r[2], "w": r[3], "h": r[4]}))
}

pub fn read_range(mid: &str, start: i64, length: i64) -> Vec<u8> {
    let c = db::conn();
    c.query_row("select substr(data, ?, ?) from media where id=?", params![start + 1, length, mid], |r| r.get::<_, Vec<u8>>(0)).unwrap_or_default()
}

pub fn url(mid: &str) -> String {
    format!("/api/media/{mid}")
}

pub fn used() -> HashSet<String> {
    db::rows(
        "select distinct json_extract(a.value, '$.media') from messages m, json_each(m.meta, '$.attachments') a
        where m.meta like '%\"attachments\"%' and json_extract(a.value, '$.media') is not null",
        [],
    )
    .into_iter()
    .map(|r| r.s(0))
    .collect()
}

pub fn gc() {
    let keep = used();
    let old: Vec<String> = db::rows("select id from media where created < ?", params![py::now() - KEEP_S]).into_iter().map(|r| r.s(0)).filter(|x| !keep.contains(x)).collect();
    for x in old {
        db::exec("delete from media where id=?", params![x]);
    }
}

pub async fn gc_loop() {
    loop {
        gc();
        tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
    }
}

pub fn clean_attachments(items: &Value) -> Vec<Value> {
    let mut out = vec![];
    for a in items.as_array().map(|a| a.iter().take(MAX_ATTACHMENTS).cloned().collect::<Vec<_>>()).unwrap_or_default() {
        if !a.is_object() {
            continue;
        }
        let mut item = json!({"name": py::head(&if a["name"].is_null() { String::new() } else { py::str_of(&a["name"]) }, 200)});
        if let Some(p) = a["path"].as_str().filter(|p| !p.trim().is_empty()) {
            if let Ok(n) = library::norm(p, None) {
                item["path"] = json!(n);
            }
        }
        if let Some(m) = info(&a["media"]) {
            item["media"] = m["id"].clone();
            item["mime"] = m["mime"].clone();
            item["w"] = m["w"].clone();
            item["h"] = m["h"].clone();
            item["size"] = m["size"].clone();
            if item["name"].as_str().unwrap_or("").is_empty() {
                item["name"] = m["name"].clone();
            }
        }
        if item.get("path").is_none() && item.get("media").is_none() {
            continue;
        }
        if item.get("size").is_none() {
            if let Some(s) = a["size"].as_i64().filter(|s| *s >= 0 && a["size"].is_i64()) {
                item["size"] = json!(s);
            }
        }
        if item["name"].as_str().unwrap_or("").is_empty() {
            item["name"] = json!(basename(item["path"].as_str().unwrap_or("")));
        }
        out.push(item);
    }
    out
}

pub fn with_marks(text: &str, attachments: &[Value]) -> String {
    let lines: Vec<String> = attachments
        .iter()
        .filter_map(|a| a["path"].as_str().filter(|p| !p.is_empty()).map(|p| format!("[附件] {p}")))
        .filter(|x| !text.contains(x.as_str()))
        .collect();
    let mut all = vec![];
    if !text.is_empty() {
        all.push(text.to_string());
    }
    all.extend(lines);
    all.join("\n")
}

pub fn link(a: &Value) -> String {
    if let Some(m) = a["media"].as_str().filter(|m| !m.is_empty()) {
        return format!("/media/{m}");
    }
    let path = a["path"].as_str().unwrap_or("");
    let q: String = url::form_urlencoded::byte_serialize(path.as_bytes()).collect::<String>().replace('+', "%20");
    format!("/files/download?path={}{}", q.replace("%2F", "/"), if py::truthy(&a["mac"]) { format!("&mac={}", py::to_int(&a["mac"]).unwrap_or(0)) } else { String::new() })
}

fn vision_tests() -> serde_json::Map<String, Value> {
    memory::setting("vision_tests").and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

pub fn remember_test(provider: &str, model: &str, seen: &str) {
    if seen != "yes" && seen != "no" {
        return;
    }
    let mut d = vision_tests();
    d.insert(format!("{provider}/{model}"), json!(seen == "yes"));
    memory::set_setting("vision_tests", &Value::Object(d));
}

pub fn tested(provider: &str, model: &str) -> Option<bool> {
    vision_tests().get(&format!("{provider}/{model}")).and_then(|v| v.as_bool())
}

pub fn can_see(provider: &str, model: &str) -> bool {
    let (p, m) = if provider.is_empty() { config::role("router") } else { (provider.to_string(), model.to_string()) };
    if let Some(t) = tested(&p, &m) {
        return t;
    }
    crate::py::truthy(&crate::prices::lookup(&p, &m)["vision"])
}

pub fn strip_images(msgs: &mut [Value]) -> bool {
    let mut hit = false;
    for m in msgs.iter_mut() {
        let Some(parts) = m["content"].as_array() else { continue };
        if parts.iter().any(|p| p["type"] == "image_url") {
            let text = parts.iter().filter(|p| p["type"] == "text").map(|p| p["text"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n\n");
            m["content"] = json!(format!("{text}\n（附带的图片没发给你：这个模型不收图片）"));
            hit = true;
        }
    }
    hit
}

pub fn image_part(mid: &str) -> Option<Value> {
    let (mime, data, _) = get(mid)?;
    if !MODEL_MIMES.contains(&mime.as_str()) {
        return None;
    }
    Some(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{}", crate::util::b64(&data))}}))
}

pub fn sent_text(att: &Value) -> String {
    let what = if py::truthy(&att["media"]) { "图片已经显示在对话里" } else { "文件卡片已经显示在对话里，用户点开就能看或下载" };
    format!("已发给用户：{}（{what}）。回复里不用再贴链接。", py::str_of(&att["name"]))
}
