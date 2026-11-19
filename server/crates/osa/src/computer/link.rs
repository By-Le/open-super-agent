//! 你的 Mac：连着服务器的桌面 App 可以让服务器上的 Bot 把任务派到它所在的 Mac 上做。对应 osa/computer/link.py（说明见那里）。
//!
//! Mac 上的小程序（agent.rs，旧版 App 里是 Python 的 agent.py）主动连到 /api/computer/link，令牌在 Authorization: Bearer 里。
//! 消息是 JSON：服务器发 {"id", "op", "args"}，Mac 回 {"id", "result"} 或 {"id", "error", "kind"}；Mac 连上先发 {"type": "hello", ...}。

use super::base::{ComputerError, R};
use crate::db::{self, RowExt};
use crate::{py, util};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use parking_lot::Mutex;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::{mpsc, oneshot};

pub const TIMEOUT: f64 = 90.0;
pub const MAX_MSG: usize = 64 << 20;
pub const OFFLINE: &str = "「{name}」没连上（那台 Mac 上的 App 没开、没联网，或者电脑睡着了）";

pub fn migrate() {
    db::batch("create table if not exists computer_links(id integer primary key, name text, token_hash text, created real, last_seen real)");
    db::add_column("computer_links", "shell", "integer not null default 0");
    db::add_column("computer_links", "device", "text");
}

pub fn offline(name: &str) -> String {
    t!(OFFLINE, name = name)
}

static ONLINE: LazyLock<Mutex<HashMap<i64, Arc<Link>>>> = LazyLock::new(Default::default);

fn hash(token: &str) -> String {
    util::sha256_hex(token.as_bytes())
}

/// 发一个新的令牌，返回 (编号, 令牌)
pub fn create(name: &str) -> (i64, String) {
    let token = util::token_urlsafe(32);
    let n = py::head(name.trim(), 80);
    let id = db::insert(
        "insert into computer_links(name, token_hash, created, last_seen) values(?,?,?,0)",
        params![if n.is_empty() { "Mac".to_string() } else { n }, hash(&token), py::now()],
    );
    (id, token)
}

pub fn check(token: &str) -> Option<(i64, String)> {
    if token.is_empty() {
        return None;
    }
    let h = hash(token);
    db::rows("select id, name, token_hash from computer_links", [])
        .into_iter()
        .find(|r| {
            let th = r.s(2);
            th.len() == h.len() && th.bytes().zip(h.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
        })
        .map(|r| (r.i(0), r.s(1)))
}

/// 登记过的 Mac（连没连着都在）。名字重了的后面加编号
pub fn rows() -> Vec<Value> {
    let online = ONLINE.lock().keys().copied().collect::<Vec<_>>();
    let mut rs: Vec<Value> = db::rows("select id, name, created, last_seen, shell from computer_links order by id", [])
        .into_iter()
        .map(|r| {
            let n = r.s(1);
            json!({"id": r[0], "name": if n.is_empty() { "Mac".to_string() } else { n }, "created": r[2], "last_seen": r[3],
                   "shell": py::truthy(&r[4]), "connected": online.contains(&r.i(0))})
        })
        .collect();
    let names: Vec<String> = rs.iter().map(|r| py::str_of(&r["name"])).collect();
    for r in rs.iter_mut() {
        let n = py::str_of(&r["name"]);
        if names.iter().filter(|x| **x == n).count() > 1 {
            r["name"] = json!(format!("{n} #{}", py::str_of(&r["id"])));
        }
    }
    rs
}

pub fn row(lid: i64) -> Option<Value> {
    rows().into_iter().find(|r| r["id"].as_i64() == Some(lid))
}

/// 派任务时按名字找那台 Mac：名字一样（不分大小写、去掉两头空格），或者写的是 #编号
pub fn find(name: &str) -> Option<Value> {
    let key = name.trim().to_lowercase();
    rows().into_iter().find(|r| py::str_of(&r["name"]).to_lowercase() == key || key == format!("#{}", py::str_of(&r["id"])))
}

pub fn set_shell(lid: i64, on: bool) {
    db::exec("update computer_links set shell=? where id=?", params![on as i64, lid]);
    changed();
}

pub async fn revoke(lid: i64) {
    db::exec("delete from computer_links where id=?", params![lid]);
    let lk = ONLINE.lock().get(&lid).cloned();
    if let Some(lk) = lk {
        lk.close(&t!("这台 Mac 已经移除"));
    }
    changed();
}

pub fn connected(lid: i64) -> Option<Arc<Link>> {
    ONLINE.lock().get(&lid).cloned()
}

fn changed() {
    super::mac::changed();
}

/// 连上来的一台 Mac
pub struct Link {
    pub id: i64,
    pub name: String,
    pub desktop: bool,
    pub perms: Value,
    pub workspace: String,
    pub can_shell: bool,
    pub can_range: bool,
    pub version: String,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    closed: AtomicBool,
    out: mpsc::UnboundedSender<Message>,
}

impl Link {
    /// 让 Mac 做一件事，等它的回复
    pub async fn call(&self, op: &str, timeout: f64, args: Value) -> R<Value> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ComputerError(offline(&self.name)));
        }
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(i, tx);
        let msg = json!({"id": i, "op": op, "args": if args.is_object() { args } else { json!({}) }});
        if self.out.send(Message::Text(py::dumps_u(&msg).into())).is_err() {
            self.pending.lock().remove(&i);
            return Err(ComputerError(offline(&self.name)));
        }
        let r = tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), rx).await;
        self.pending.lock().remove(&i);
        let r = match r {
            Err(_) => return Err(ComputerError(t!("「{name}」没有回应（超时）", name = self.name.clone()))),
            Ok(Err(_)) => return Err(ComputerError(offline(&self.name))),
            Ok(Ok(r)) => r,
        };
        if let Some(e) = r.get("error") {
            return Err(ComputerError(py::str_of(e)));
        }
        Ok(r.get("result").cloned().unwrap_or(Value::Null))
    }

    fn resolve(&self, msg: Value) {
        let id = py::to_int(&msg["id"]).unwrap_or(0);
        if let Some(tx) = self.pending.lock().remove(&id) {
            let _ = tx.send(msg);
        }
    }

    pub fn close(&self, why: &str) {
        self.closed.store(true, Ordering::SeqCst);
        let msg = if why.is_empty() { offline(&self.name) } else { why.to_string() };
        for (_, tx) in self.pending.lock().drain() {
            let _ = tx.send(json!({"error": msg}));
        }
        let code = if why.is_empty() { 1000 } else { 4000 };
        let _ = self.out.send(Message::Close(Some(CloseFrame { code, reason: "".into() })));
    }
}

/// /api/computer/link：Mac 上的小程序连进来
pub async fn ws(h: HeaderMap, up: WebSocketUpgrade) -> Response {
    let token = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let token = token.strip_prefix("Bearer ").unwrap_or(&token).trim().to_string();
    up.max_message_size(MAX_MSG).max_frame_size(MAX_MSG).on_upgrade(move |s| serve(s, token))
}

async fn serve(sock: WebSocket, token: String) {
    use futures::{SinkExt, StreamExt};
    let (mut sink, mut stream) = sock.split();
    // 先接受再关：握手时就拒绝的话，Mac 那边只看到 403，拿不到原因码
    let Some((rid, rname)) = check(&token) else {
        let _ = sink.send(Message::Close(Some(CloseFrame { code: 4401, reason: "".into() }))).await;
        return;
    };
    let hello = match tokio::time::timeout(std::time::Duration::from_secs(20), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).unwrap_or(Value::Null),
        Ok(Some(Ok(Message::Binary(b)))) => serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    if hello["type"] != "hello" {
        let _ = sink.send(Message::Close(Some(CloseFrame { code: 1008, reason: "".into() }))).await;
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let name = py::head(&if py::truthy(&hello["name"]) { py::str_of(&hello["name"]) } else { rname }, 80);
    let lk = Arc::new(Link {
        id: rid,
        name: name.clone(),
        desktop: py::truthy(&hello["desktop"]),
        perms: if hello["perms"].is_object() { hello["perms"].clone() } else { json!({}) },
        workspace: if hello["workspace"].is_null() { String::new() } else { py::str_of(&hello["workspace"]) },
        can_shell: py::truthy(&hello["shell"]),
        can_range: py::truthy(&hello["ranges"]),
        version: if hello["version"].is_null() { String::new() } else { py::str_of(&hello["version"]) },
        next: AtomicI64::new(1),
        pending: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
        out: tx,
    });
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
            }
        }
    });
    let old = ONLINE.lock().insert(rid, lk.clone());
    if let Some(o) = old {
        o.close(&t!("重新连上了"));
    }
    let device = py::head(&if hello["device"].is_null() { String::new() } else { py::str_of(&hello["device"]) }, 64);
    db::exec(
        "update computer_links set last_seen=?, name=?, device=coalesce(nullif(?, ''), device) where id=?",
        params![py::now(), name, device, rid],
    );
    if !device.is_empty() {
        replace_older(rid, &device);
    }
    changed();
    while let Some(Ok(msg)) = stream.next().await {
        let v = match msg {
            Message::Text(t) => serde_json::from_str::<Value>(&t).ok(),
            Message::Binary(b) => serde_json::from_slice::<Value>(&b).ok(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Some(v) = v else { break };
        if v.get("id").is_some() {
            lk.resolve(v);
        }
    }
    lk.close("");
    let mine = {
        let mut on = ONLINE.lock();
        if on.get(&rid).is_some_and(|x| Arc::ptr_eq(x, &lk)) {
            on.remove(&rid);
            true
        } else {
            false
        }
    };
    if mine {
        db::exec("update computer_links set last_seen=? where id=?", params![py::now(), rid]);
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    changed();
}

/// 同一台 Mac（device 一样）以前登记的行：删掉，令牌随之作废，还连着的断开
fn replace_older(lid: i64, device: &str) {
    let old: Vec<i64> =
        db::rows("select id from computer_links where device=? and id<>?", params![device, lid]).into_iter().map(|r| r.i(0)).collect();
    for i in &old {
        db::exec("delete from computer_links where id=?", params![i]);
    }
    for i in old {
        let lk = ONLINE.lock().remove(&i);
        if let Some(lk) = lk {
            lk.close(&t!("这台 Mac 已经用新的令牌连上了"));
        }
    }
}
