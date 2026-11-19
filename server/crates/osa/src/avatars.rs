//! 头像图片：Bot 和群聊可以用自己的图片当头像（上传的，或者让生图模型画的）。对应 osa/avatars.py（说明见那里）。
//!
//! 用哪个模型画、怎么画在 images 模块（Python 版也是 avatars 和 images 共用），这里的 choice / choose / generate 转给它。

use crate::db::{self, RowExt};
use crate::py;
use crate::web::{Body, Res, ok};
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use regex::Regex;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

pub const MAX_BYTES: usize = 512 * 1024;
pub const KEEP_S: f64 = 86400.0;
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/avatars/([0-9a-f]{24})$").unwrap());

pub fn migrate() {
    db::batch("create table if not exists avatars(id text primary key, mime text, data blob, created real)");
}

fn mime_of(b: &[u8]) -> Option<&'static str> {
    crate::media::sniff(b).filter(|m| ["image/png", "image/jpeg", "image/webp"].contains(m))
}

/// 存一张头像（data:image/...;base64,...），返回它的地址
pub fn save(data_url: &str) -> Result<String, String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^data:image/[\w.+-]+;base64,(.+)$").unwrap());
    let b = RE
        .captures(data_url)
        .and_then(|c| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(c[1].as_bytes()).ok()
        })
        .unwrap_or_default();
    if b.is_empty() {
        return Err(t!("没收到图片"));
    }
    if b.len() > MAX_BYTES {
        return Err(t!("图片太大了（最多 {n} KB）", n = MAX_BYTES / 1024));
    }
    let mime = mime_of(&b).ok_or_else(|| t!("只支持 PNG、JPG、WebP 图片"))?;
    let aid = py::head(&crate::util::sha256_hex(&b), 24);
    db::exec(
        "insert into avatars(id, mime, data, created) values(?,?,?,?) on conflict(id) do update set created=excluded.created",
        params![aid, mime, b, py::now()],
    );
    Ok(format!("/avatars/{aid}"))
}

pub fn get_avatar(aid: &str) -> Option<(String, Vec<u8>)> {
    let c = db::conn();
    c.query_row("select mime, data from avatars where id=?", params![aid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))).ok()
}

pub fn valid(url: &str) -> bool {
    URL.captures(url).is_some_and(|m| db::exists("select 1 from avatars where id=?", params![&m[1]]))
}

/// 清掉一天前传上来、现在没有 Bot 或群聊在用的图片
pub fn gc() {
    let mut used = HashSet::new();
    for r in db::rows("select avatar from bots", []) {
        let a = r.j(0);
        if let Some(m) = URL.captures(&if a["image"].is_null() { String::new() } else { py::str_of(&a["image"]) }) {
            used.insert(m[1].to_string());
        }
    }
    for r in db::rows("select image from chats where image is not null", []) {
        if let Some(m) = URL.captures(&r.s(0)) {
            used.insert(m[1].to_string());
        }
    }
    let old: Vec<String> = db::rows("select id from avatars where created < ?", params![py::now() - KEEP_S])
        .into_iter()
        .map(|r| r.s(0))
        .filter(|x| !used.contains(x))
        .collect();
    for x in old {
        db::exec("delete from avatars where id=?", params![x]);
    }
}

// ---------- 用哪个模型画（选择、列表、画都在 images 模块，这里转一下，和 Python 版 avatars.py 的接口一样） ----------
pub fn choose(provider: &str, model: &str, edit: bool) {
    crate::images::choose(provider, model, edit);
}

pub fn choice(edit: bool) -> Value {
    crate::images::choice(edit)
}

/// 让生图模型画一张头像，返回原图的 data URL（浏览器再裁成 256 像素传回来）
pub async fn generate(prompt: &str, provider: &str, model: &str) -> Result<String, String> {
    if prompt.trim().is_empty() {
        return Err(t!("写一句想要什么样的头像"));
    }
    let (b, mime) = crate::images::draw(prompt, provider, model, &[]).await?;
    Ok(format!("data:{mime};base64,{}", crate::util::b64(&b)))
}

// ---------- 接口 ----------
async fn api_upload(Body(d): Body) -> Res {
    let data = if py::truthy(&d["data"]) { py::str_of(&d["data"]) } else { String::new() };
    ok(json!({"image": save(&data)?}))
}

async fn api_models() -> Res {
    ok(crate::images::models().await)
}

async fn api_generate(Body(d): Body) -> Res {
    let s = |k: &str| if py::truthy(&d[k]) { py::str_of(&d[k]) } else { String::new() };
    ok(json!({"data": generate(&s("prompt"), &s("provider"), &s("model")).await?}))
}

async fn api_file(Path(aid): Path<String>) -> Response {
    let got = if aid.len() == 24 { get_avatar(&aid) } else { None };
    let Some((mime, data)) = got else {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"detail": "Not Found"}))).into_response();
    };
    let mut r = Response::new(axum::body::Body::from(data));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'"));
    r
}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/avatars", post(api_upload))
        .route("/api/avatars/models", get(api_models))
        .route("/api/avatars/generate", post(api_generate))
        .route("/avatars/{aid}", get(api_file))
}
