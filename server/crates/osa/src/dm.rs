//! Bot 之间的私信：一个 Bot 直接请另一个 Bot 帮忙，不用拉群。回信自动送回。对应 osa/dm.py（规则见那里的说明）。

use crate::task::{Reply, Task};
use crate::{bots, groups, memory, providers, py, router, server, t};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};

pub const MAX_DEPTH: i64 = 5;

/// 叫醒的原因
#[derive(Clone, Debug)]
pub struct Woke {
    pub kind: String,
    pub from: i64,
    pub text: String,
    pub edited: Vec<String>,
}

/// 这一轮的来由
#[derive(Clone, Default)]
pub struct Turn {
    pub reply: Option<Reply>,
    pub depth: Arc<AtomicI64>,
    pub woke: Option<Woke>,
    pub started: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Turn {
    pub fn with_depth(depth: i64) -> Turn {
        Turn { depth: Arc::new(AtomicI64::new(depth)), ..Default::default() }
    }
    pub fn depth(&self) -> i64 {
        self.depth.load(Ordering::Relaxed)
    }
}

type Key = (i64, i64, String, i64, Option<Vec<(i64, i64)>>);
static QUEUED: LazyLock<Mutex<HashMap<Key, Turn>>> = LazyLock::new(Default::default);

pub fn name_of(bot_id: Option<i64>) -> String {
    bot_id.and_then(bots::get).map(|b| py::str_of(&b["name"])).unwrap_or_else(|| t!("已删除的 Bot"))
}

fn message(chat_id: i64, content: &str, bot_id: i64, meta: Value) -> i64 {
    let mid = memory::add("task", content, chat_id, Some(bot_id), &meta);
    server::broadcast(json!({"type": "message", "chat_id": chat_id, "bot_id": bot_id, "message": {
        "id": mid, "ts": py::now(), "role": "task", "content": content, "meta": meta, "bot_id": bot_id}}));
    mid
}

pub async fn send(sender: i64, to: &Value, text: &str, origin: i64, owed: Option<Reply>, depth: i64) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "没发：私信内容是空的".into();
    }
    if depth + 1 > MAX_DEPTH {
        return format!("没发：Bot 之间已经转了 {MAX_DEPTH} 手，先停下来，把现在的情况告诉用户，等用户决定");
    }
    let to_id = to["id"].as_i64().unwrap_or(0);
    let to_chat = to["chat_id"].as_i64().unwrap_or(0);
    let reply = Reply { bot: sender, chat: origin, depth: depth + 1, then: owed.map(Box::new) };
    message(origin, text, sender, json!({"kind": "dm", "to": to_id}));
    message(to_chat, text, sender, json!({"kind": "dm", "to": to_id, "chat": origin}));
    let turn = Turn {
        reply: Some(reply),
        depth: Arc::new(AtomicI64::new(depth + 1)),
        woke: Some(Woke { kind: "dm".into(), from: sender, text: text.into(), edited: vec![] }),
        started: None,
    };
    tokio::spawn(wake(to_id, to_chat, turn));
    format!("已私信 {}。它的回信会自动回到这个对话，到时你会接着说一轮，现在不用等", py::str_of(&to["name"]))
}

pub async fn answer(sender: i64, reply: &Reply, text: &str, from_chat: i64, extra: Value) {
    let Some(to) = bots::get(reply.bot) else {
        message(from_chat, &t!("[等回信的 Bot 已经删除，回信没有送出]"), sender, json!({"kind": "note"}));
        return;
    };
    let to_id = to["id"].as_i64().unwrap_or(0);
    let chat = bots::chat(reply.chat);
    let cid = match chat {
        Some(c) if c["kind"] != "group" || bots::ids_of(&c).contains(&to_id) => reply.chat,
        _ => to["chat_id"].as_i64().unwrap_or(0),
    };
    message(from_chat, &t!("[已回复 {name}]", name = py::str_of(&to["name"])), sender, json!({"kind": "dm_reply", "to": to_id, "sent": true, "chat": cid}));
    let mut meta = json!({"kind": "dm_reply", "to": to_id});
    if let Some(e) = extra.as_object() {
        for (k, v) in e {
            meta[k] = v.clone();
        }
    }
    message(cid, text, sender, meta);
    let turn = Turn {
        reply: reply.then.as_deref().cloned(),
        depth: Arc::new(AtomicI64::new(reply.depth)),
        woke: Some(Woke { kind: "reply".into(), from: sender, text: text.into(), edited: vec![] }),
        started: None,
    };
    tokio::spawn(wake(to_id, cid, turn));
}

pub async fn after_task(task: Arc<Task>) {
    let (status, result, reply) = {
        let s = task.st.lock();
        (s.status.clone(), s.result.clone(), s.reply.clone())
    };
    let Some(reply) = reply else { return };
    let text = if status == "done" {
        result
    } else {
        t!("（任务 #{id} 没做完。{result}）", id = task.id, result = if result.is_empty() { status.clone() } else { result })
    };
    answer(task.bot_id, &reply, &text, task.chat_id, json!({"task_id": task.id, "status": status})).await;
}

fn owed(r: &Option<Reply>) -> Option<Vec<(i64, i64)>> {
    let mut out = vec![];
    let mut cur = r.as_ref();
    while let Some(x) = cur {
        out.push((x.bot, x.chat));
        cur = x.then.as_deref();
    }
    if r.is_some() { Some(out) } else { None }
}

pub fn same(a: &Option<Reply>, b: &Option<Reply>) -> bool {
    owed(a) == owed(b)
}

pub async fn wake(bot_id: i64, chat_id: i64, mut turn: Turn) {
    let Some(chat) = bots::chat(chat_id) else { return };
    if bots::get(bot_id).is_none() {
        return;
    }
    let woke = turn.woke.clone().unwrap_or(Woke { kind: String::new(), from: 0, text: String::new(), edited: vec![] });
    let key: Key = (bot_id, chat_id, woke.kind.clone(), woke.from, owed(&turn.reply));
    {
        let mut q = QUEUED.lock();
        if let Some(existing) = q.get(&key) {
            let d = existing.depth().max(turn.depth());
            existing.depth.store(d, Ordering::Relaxed);
            return;
        }
        let k2 = key.clone();
        let marker = turn.depth.clone();
        turn.started = Some(Arc::new(move || {
            let mut q = QUEUED.lock();
            if q.get(&k2).is_some_and(|t| Arc::ptr_eq(&t.depth, &marker)) {
                q.remove(&k2);
            }
        }));
        q.insert(key.clone(), turn.clone());
    }
    let r = if chat["kind"] == "group" {
        groups::wake(chat_id, bot_id, turn.clone()).await;
        Ok(())
    } else {
        router::runtime(bot_id).respond(chat_id, turn.clone()).await
    };
    if let Err(e) = r {
        tracing::error!("对话 {chat_id} 里 Bot {bot_id} 处理私信时出错：{e:#}");
        server::broadcast(json!({"type": "error", "chat_id": chat_id, "message": t!("{name} 出错了：{e}", name = name_of(Some(bot_id)), e = providers::brief(&e))}));
    }
    let mut q = QUEUED.lock();
    if q.get(&key).is_some_and(|t| Arc::ptr_eq(&t.depth, &turn.depth)) {
        q.remove(&key);
    }
}
