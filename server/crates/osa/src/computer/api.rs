//! 电脑的接口：右栏的电脑面板、接手 / 交还、Instabox 的机器和 API key、看哪个任务、VNC 画面转发、资料库里电脑里的文件。
//! 对应 osa/server.py 里电脑和文件那两段。派任务到你的 Mac（/api/computer/links、/api/computer/macs…、/api/computer/link）是第 2 步 B。

use super::base::{Computer, Vnc};
use super::{HOSTS, get, public_state};
use crate::web::{ApiError, Body, Q, Res, ok, qi, qs};
use crate::{approvals, bots, library, py, security, server};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get as rget, post, put};
use futures::{SinkExt, StreamExt};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError::bad(e.to_string())
}

fn pc(bot_id: i64) -> Result<Arc<Computer>, ApiError> {
    if bots::get(bot_id).is_none() {
        return Err(ApiError::not_found(t!("没有这个 Bot")));
    }
    Ok(get(bot_id))
}

fn qbool(q: &HashMap<String, String>, k: &str) -> bool {
    q.get(k).is_some_and(|v| ["1", "true", "yes", "on", "t", "y"].contains(&v.trim().to_lowercase().as_str()))
}

async fn pc_status(Path(bot_id): Path<i64>) -> Res {
    pc(bot_id)?;
    ok(public_state(bot_id).await)
}

async fn pc_shot(Path(bot_id): Path<i64>, q: Q) -> Response {
    let c = match pc(bot_id) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match c.backend.peek(&c, "jpeg", qs(&q, "view").unwrap_or("")).await {
        Ok(data) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")], data).into_response(),
        Err(e) => ApiError::new(409, e.0).into_response(),
    }
}

async fn pc_type(Path(bot_id): Path<i64>, Body(d): Body) -> Res {
    let c = pc(bot_id)?;
    if c.controller() != "human" {
        return Err(ApiError::bad(t!("先接手电脑再输入")));
    }
    if py::truthy(&d["text"]) {
        c.type_text(&py::head(&py::str_of(&d["text"]), 2000), true).await.map_err(bad)?;
    }
    if py::truthy(&d["key"]) {
        c.press(&py::str_of(&d["key"]), true).await.map_err(bad)?;
    }
    ok(json!({"ok": true}))
}

async fn broadcast_state(bot_id: i64) -> Value {
    let st = public_state(bot_id).await;
    server::broadcast(json!({"type": "computer", "computer": st}));
    st
}

async fn pc_action(Path((bot_id, action)): Path<(i64, String)>, Body(d): Body) -> Res {
    let c = pc(bot_id)?;
    let r: Result<(), ApiError> = async {
        match action.as_str() {
            "start" => c.start().await.map_err(bad)?,
            "pause" => c.pause().await.map_err(bad)?,
            "takeover" => {
                let desk = HOSTS.contains(&c.kind()) && c.backend.desk_busy();
                if !desk {
                    c.start().await.map_err(bad)?;
                }
                let tid = d["task_id"].clone();
                let tid_s = if tid.is_null() { String::new() } else { py::str_of(&tid) };
                let key = {
                    let s = c.st.lock();
                    s.asking.get(&tid_s).cloned().or_else(|| {
                        if py::truthy(&tid) { s.users.iter().find(|(_, u)| u.task_id == tid).map(|(k, _)| k.clone()) } else { None }
                    })
                };
                let has_window = key.as_ref().is_some_and(|k| c.st.lock().users.get(k).is_some_and(|u| py::truthy(&u.window)));
                if has_window {
                    c.front(key.as_deref().unwrap_or("")).await.map_err(bad)?;
                } else if HOSTS.contains(&c.kind()) && !desk {
                    c.backend.show(&c).await.map_err(bad)?;
                }
                c.takeover();
            }
            "show" if HOSTS.contains(&c.kind()) => c.backend.show(&c).await.map_err(bad)?,
            "handback" => return Ok(()),
            _ => return Err(ApiError::not_found(t!("未知操作"))),
        }
        Ok(())
    }
    .await;
    r?;
    if action == "handback" {
        return ok(handback(bot_id).await);
    }
    ok(broadcast_state(bot_id).await)
}

/// 交还电脑。如果 Bot 正在等你帮忙，交还就表示处理完了
pub async fn handback(bot_id: i64) -> Value {
    get(bot_id).handback();
    for a in approvals::pending_public() {
        if a["bot_id"].as_i64() == Some(bot_id) && a["kind"] == "help" {
            approvals::resolve(a["id"].as_i64().unwrap_or(0), true, "", false, &Value::Null, None, None);
        }
    }
    broadcast_state(bot_id).await
}

async fn pc_desktop_grant() -> Res {
    let c = get(0);
    if !HOSTS.contains(&c.kind()) || !c.backend.desk_supported() {
        return Err(ApiError::bad(t!("只有在 Mac 上用本机电脑时才能操作整台电脑")));
    }
    c.backend.grant(&c).await.map_err(bad)?;
    ok(broadcast_state(0).await)
}

async fn pc_machines() -> Res {
    let c = get(0);
    match c.backend.list_machines(&c).await {
        None => ok(json!({"backend": c.kind(), "machines": [], "fixed": false})),
        Some(Err(e)) => Err(bad(e)),
        Some(Ok(m)) => ok(json!({"backend": c.kind(), "machines": m, "fixed": c.backend.fixed_machine()})),
    }
}

async fn pc_use(Body(d): Body) -> Res {
    let c = get(0);
    let mid = d["machine_id"].as_str().unwrap_or("").trim().to_string();
    let supports = c.kind() == "instabox";
    if !supports || mid.is_empty() {
        return Err(ApiError::bad(t!("只有 Instabox 后端能选已有的机器")));
    }
    let busy = {
        let s = c.st.lock();
        !s.users.is_empty() || !s.exclusive.is_empty()
    };
    if busy {
        return Err(ApiError::bad(t!("电脑正在被用着（{who}），等用完再换", who = c.busy())));
    }
    let list = c.backend.list_machines(&c).await.unwrap_or(Ok(vec![])).map_err(bad)?;
    if !list.iter().any(|m| m["id"].as_str() == Some(mid.as_str())) {
        return Err(ApiError::not_found(t!("账号下没有这台机器")));
    }
    if let Some(Err(e)) = c.backend.use_machine(&c, &mid) {
        return Err(bad(e));
    }
    let bot_id = py::to_int(&d["bot_id"]).filter(|b| *b != 0).or_else(|| bots::primary().and_then(|p| p["id"].as_i64())).unwrap_or(0);
    ok(broadcast_state(bot_id).await)
}

async fn pc_instabox_key(Body(d): Body) -> Res {
    if security::demo() {
        return Err(ApiError::new(403, t!("演示服务器上不能换电脑")));
    }
    if !super::choices().contains(&"instabox") {
        return Err(ApiError::new(403, t!("这台服务器上不能选这种电脑（config.yaml 的 computer.choices）")));
    }
    let c = get(0);
    let busy = {
        let s = c.st.lock();
        !s.users.is_empty() || !s.exclusive.is_empty()
    };
    if c.kind() == "instabox" && busy {
        return Err(ApiError::bad(t!("电脑正在被用着（{who}），等用完再换", who = c.busy())));
    }
    let k = if d["key"].is_null() { String::new() } else { py::str_of(&d["key"]) };
    super::instabox::save_key(&k).await.map_err(bad)?;
    server::broadcast(json!({"type": "computer", "computer": public_state(0).await}));
    ok(crate::settings::public())
}

async fn pc_watch(Body(d): Body) -> Res {
    let c = get(0);
    let key = d["key"].as_str().filter(|k| !k.is_empty()).map(String::from);
    match key {
        Some(k) => {
            let has = c.st.lock().users.get(&k).is_some_and(|u| py::truthy(&u.window));
            if !has {
                return Err(ApiError::not_found(t!("这个任务已经不用电脑了，或者还没打开浏览器")));
            }
            c.front(&k).await.map_err(bad)?;
        }
        None => c.st.lock().watch = None,
    }
    ok(broadcast_state(0).await)
}

async fn pc_macs() -> Res {
    ok(json!(super::macs_state().await))
}

/// 网页上的 noVNC ⇄ 电脑的 VNC。只做字节转发
async fn pc_vnc(Path(bot_id): Path<i64>, h: HeaderMap, up: WebSocketUpgrade) -> Response {
    let proto = h.get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).unwrap_or("").contains("binary");
    let up = if proto { up.protocols(["binary"]) } else { up };
    up.on_upgrade(move |sock| vnc_run(sock, bot_id))
}

async fn vnc_run(sock: WebSocket, bot_id: i64) {
    let c = get(bot_id);
    let (mut sink, mut stream) = sock.split();
    let vnc = match async {
        c.start().await?;
        c.open_vnc().await
    }
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let _ = sink
                .send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 1011, reason: py::head(&e.0, 100).into() })))
                .await;
            return;
        }
    };
    let c2 = c.clone();
    match vnc {
        Vnc::Io { mut rx, mut tx, child } => {
            let up = async {
                while let Some(Ok(m)) = stream.next().await {
                    let data: Vec<u8> = match m {
                        Message::Binary(b) => b.to_vec(),
                        Message::Text(t) => t.as_bytes().to_vec(),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if !data.is_empty() {
                        c2.touch();
                        if tx.write_all(&data).await.is_err() {
                            break;
                        }
                        let _ = tx.flush().await;
                    }
                }
                let _ = tx.shutdown().await;
            };
            let down = async {
                let mut buf = vec![0u8; 65536];
                loop {
                    match rx.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sink.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            };
            tokio::select! { _ = up => {}, _ = down => {} }
            if let Some(mut ch) = child {
                if tokio::time::timeout(std::time::Duration::from_secs(5), ch.wait()).await.is_err() {
                    let _ = ch.kill().await;
                }
            }
        }
        Vnc::Ws(ws) => {
            use tokio_tungstenite::tungstenite::Message as TM;
            let (mut wtx, mut wrx) = ws.split();
            let up = async {
                while let Some(Ok(m)) = stream.next().await {
                    let data: Vec<u8> = match m {
                        Message::Binary(b) => b.to_vec(),
                        Message::Text(t) => t.as_bytes().to_vec(),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if !data.is_empty() {
                        c2.touch();
                        if wtx.send(TM::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                }
                let _ = wtx.close().await;
            };
            let down = async {
                while let Some(Ok(m)) = wrx.next().await {
                    let data: Vec<u8> = match m {
                        TM::Binary(b) => b.to_vec(),
                        TM::Text(t) => t.as_bytes().to_vec(),
                        TM::Close(_) => break,
                        _ => continue,
                    };
                    if sink.send(Message::Binary(data.into())).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! { _ = up => {}, _ = down => {} }
        }
    }
}

// ---------- 资料库：电脑里的文件 ----------
/// 文件都在共用电脑里。start=false 时电脑没开就返回 None，不替用户开机
async fn files_pc(start: bool) -> Result<Option<Arc<Computer>>, ApiError> {
    let c = get(0);
    if c.state().await != "running" {
        if !start {
            return Ok(None);
        }
        c.start().await.map_err(bad)?;
    }
    Ok(Some(c))
}

async fn list_files(q: Q) -> Res {
    let p = library::norm(qs(&q, "path").unwrap_or(""), None).map_err(ApiError::bad)?;
    let Some(c) = files_pc(qbool(&q, "start")).await? else {
        return ok(json!({"running": false, "state": get(0).state().await, "path": p, "root": library::root()}));
    };
    let entries = library::list_dir(&c, &p).await.map_err(ApiError::bad)?;
    let pinned = library::pins();
    let stats = library::stat_many(&c, &pinned).await.map_err(ApiError::bad)?;
    let pins: Vec<Value> =
        pinned.iter().map(|x| stats.get(x).cloned().unwrap_or_else(|| json!({"name": super::base::basename(x), "path": x, "missing": true}))).collect();
    ok(json!({"running": true, "path": p, "root": library::root(), "entries": entries, "pins": pins}))
}

pub const CHUNK: u64 = 4 << 20;

fn media_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|x| x.1.to_lowercase()).unwrap_or_default();
    Some(match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        _ => return None,
    })
}

/// Range 请求头 → (起, 止)，止也算在内。None：整个发；Some(None)：超出文件（416）
fn byte_range(h: Option<&str>, size: u64) -> Option<Option<(u64, u64)>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*bytes=(\d*)-(\d*)\s*$").unwrap());
    let c = RE.captures(h?)?;
    let (a, b) = (&c[1], &c[2]);
    if a.is_empty() && b.is_empty() {
        return None;
    }
    if !a.is_empty() && !b.is_empty() && b.parse::<u64>().ok()? < a.parse::<u64>().ok()? {
        return None;
    }
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        return Some(if n > 0 && size > 0 { Some((size.saturating_sub(n), size - 1)) } else { None });
    }
    let start: u64 = a.parse().ok()?;
    if start >= size {
        return Some(None);
    }
    let end = if b.is_empty() { size - 1 } else { b.parse::<u64>().ok()?.min(size - 1) };
    Some(Some((start, end)))
}

fn quote_url(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn download_file(q: Q, h: HeaderMap) -> Response {
    match download(q, h).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn download(q: Q, h: HeaderMap) -> Result<Response, ApiError> {
    let path = qs(&q, "path").ok_or_else(|| ApiError::new(422, "Field required: path"))?.to_string();
    let mac = qi(&q, "mac").unwrap_or(0);
    let force = qbool(&q, "download");
    if mac != 0 {
        // 派到你的 Mac 上的任务发的文件（第 2 步 B）
        return Err(ApiError::not_found(t!("这台 Mac 已经移除")));
    }
    let p = library::norm(&path, None).map_err(ApiError::bad)?;
    let c = files_pc(true).await?.ok_or_else(|| ApiError::bad("电脑没开"))?;
    let st = c.stat_paths(std::slice::from_ref(&p)).await.map_err(bad)?.get(&p).cloned();
    let Some(st) = st.filter(|s| s["dir"] != true) else {
        return Err(ApiError::not_found(t!("文件不存在：{path}", path = p.clone())));
    };
    let size = py::to_int(&st["size"]).unwrap_or(0).max(0) as u64;
    let name = super::base::basename(&p);
    let kind = media_type(&name).map(String::from).unwrap_or_else(|| mime_guess::from_path(&name).first().map(|m| m.essence_str().to_string()).unwrap_or_else(|| "application/octet-stream".into()));
    let inline = !force
        && (((kind.starts_with("image/") || kind.starts_with("video/") || kind.starts_with("audio/")) && kind != "image/svg+xml") || kind == "application/pdf" || kind == "text/plain");
    let mtime = st["mtime"].as_f64().unwrap_or(0.0) as i64;
    let etag = format!("\"{size:x}-{mtime:x}\"");
    let mut hdrs: Vec<(String, String)> = vec![
        ("Content-Disposition".into(), format!("{}; filename*=UTF-8''{}", if inline { "inline" } else { "attachment" }, quote_url(&name).replace('/', "%2F"))),
        ("X-Content-Type-Options".into(), "nosniff".into()),
        ("Accept-Ranges".into(), "bytes".into()),
        ("ETag".into(), etag.clone()),
        ("Cache-Control".into(), "private, no-cache".into()),
    ];
    let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok());
    if hv("if-none-match") == Some(etag.as_str()) {
        return Ok(Response::builder().status(304).header("ETag", etag).header("Cache-Control", "private, no-cache").body(axum::body::Body::empty()).unwrap());
    }
    let rng = match hv("if-range") {
        Some(ir) if ir != etag => None,
        _ => byte_range(hv("range"), size),
    };
    if rng == Some(None) {
        return Ok(Response::builder()
            .status(416)
            .header("Content-Range", format!("bytes */{size}"))
            .header("Accept-Ranges", "bytes")
            .body(axum::body::Body::empty())
            .unwrap());
    }
    let ctype = if inline { kind.clone() } else { "application/octet-stream".into() };
    let ctype = if ctype.starts_with("text/") { format!("{ctype}; charset=utf-8") } else { ctype };
    let build = |status: u16, hdrs: &[(String, String)], body: axum::body::Body| {
        let mut b = Response::builder().status(status).header(header::CONTENT_TYPE, ctype.clone());
        for (k, v) in hdrs {
            b = b.header(k.as_str(), v.as_str());
        }
        b.body(body).unwrap()
    };
    if size == 0 {
        return Ok(build(200, &hdrs, axum::body::Body::empty()));
    }
    let (start, end) = rng.flatten().unwrap_or((0, size - 1));
    let first = c.read_range(&p, start, CHUNK.min(end - start + 1)).await.map_err(bad)?;
    let c2 = c.clone();
    let p2 = p.clone();
    let stream = async_stream(c2, p2, first, start, end);
    hdrs.push(("Content-Length".into(), (end - start + 1).to_string()));
    let partial = rng.flatten().is_some();
    if partial {
        hdrs.push(("Content-Range".into(), format!("bytes {start}-{end}/{size}")));
    }
    Ok(build(if partial { 206 } else { 200 }, &hdrs, axum::body::Body::from_stream(stream)))
}

fn async_stream(c: Arc<Computer>, p: String, first: Vec<u8>, start: u64, end: u64) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    futures::stream::unfold((Some(first), start, false), move |(pending, pos, done)| {
        let c = c.clone();
        let p = p.clone();
        async move {
            if done {
                return None;
            }
            if let Some(f) = pending {
                let n = f.len() as u64;
                let stop = f.is_empty();
                return Some((Ok(bytes::Bytes::from(f)), (None, pos + n, stop)));
            }
            if pos > end {
                return None;
            }
            match c.read_range(&p, pos, CHUNK.min(end - pos + 1)).await {
                Ok(d) if !d.is_empty() => {
                    let n = d.len() as u64;
                    Some((Ok(bytes::Bytes::from(d)), (None, pos + n, false)))
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("读 {p} 到 {pos} 字节时出错：{e}");
                    None
                }
            }
        }
    })
}

async fn upload_file(q: Q, req: Request) -> Res {
    let dir = qs(&q, "dir").unwrap_or("").to_string();
    let name = qs(&q, "name").unwrap_or("").to_string();
    library::norm(&dir, None).map_err(ApiError::bad)?;
    library::check_name(&name).map_err(ApiError::bad)?;
    let data = axum::body::to_bytes(req.into_body(), library::MAX_UPLOAD + 1).await.map_err(|_| ApiError::new(413, t!("文件太大（最多 {n} MB）", n = library::MAX_UPLOAD >> 20)))?;
    if data.len() > library::MAX_UPLOAD {
        return Err(ApiError::new(413, t!("文件太大（最多 {n} MB）", n = library::MAX_UPLOAD >> 20)));
    }
    let c = files_pc(true).await?.ok_or_else(|| ApiError::bad("电脑没开"))?;
    ok(library::put_file(&c, &dir, &name, &data).await.map_err(ApiError::bad)?)
}

async fn delete_file(q: Q) -> Res {
    let path = qs(&q, "path").ok_or_else(|| ApiError::new(422, "Field required: path"))?.to_string();
    let c = files_pc(true).await?.ok_or_else(|| ApiError::bad("电脑没开"))?;
    library::remove(&c, &path).await.map_err(ApiError::bad)?;
    ok(json!({"ok": true}))
}

#[allow(dead_code)]
fn _h(_: HeaderValue, _: StatusCode) {}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/bots/{bot_id}/computer", rget(pc_status))
        .route("/api/bots/{bot_id}/computer/shot", rget(pc_shot))
        .route("/api/bots/{bot_id}/computer/type", post(pc_type))
        .route("/api/bots/{bot_id}/computer/{action}", post(pc_action))
        .route("/api/bots/{bot_id}/computer/vnc", rget(pc_vnc))
        .route("/api/computer/desktop/grant", post(pc_desktop_grant))
        .route("/api/computer/machines", rget(pc_machines))
        .route("/api/computer/use", post(pc_use))
        .route("/api/computer/instabox-key", put(pc_instabox_key))
        .route("/api/computer/watch", post(pc_watch))
        .route("/api/computer/macs", rget(pc_macs))
        .route("/api/files", rget(list_files).delete(delete_file))
        .route("/api/files/download", rget(download_file))
        .route("/files/download", rget(download_file))
        .route("/api/files/content", put(upload_file))
}
