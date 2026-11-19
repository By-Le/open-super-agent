//! 群聊：2–6 个 Bot 在同一个对话里。这里决定每条消息轮到谁说。对应 osa/groups.py（规则见那里的说明）。

use crate::dm::Turn;
use crate::task::Task;
use crate::{bots, memory, providers, py, router, server};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

pub const MAX_TURNS: usize = 8;
pub const EVERYONE: [&str; 3] = ["everyone", "所有人", "全体成员"];

pub struct Group {
    pub chat_id: i64,
    pub lock: tokio::sync::Mutex<()>,
    pub handoffs: Mutex<Vec<i64>>,
    pub expected: Mutex<HashMap<i64, i64>>,
}

static GROUPS: LazyLock<Mutex<HashMap<i64, Arc<Group>>>> = LazyLock::new(Default::default);

pub fn get(chat_id: i64) -> Arc<Group> {
    GROUPS
        .lock()
        .entry(chat_id)
        .or_insert_with(|| Arc::new(Group { chat_id, lock: tokio::sync::Mutex::new(()), handoffs: Mutex::new(vec![]), expected: Mutex::new(HashMap::new()) }))
        .clone()
}

pub fn forget(chat_id: i64) {
    GROUPS.lock().remove(&chat_id);
}

fn name_of(bot_id: i64) -> String {
    bots::get(bot_id).map(|b| py::str_of(&b["name"])).unwrap_or_else(|| t!("已删除的 Bot"))
}

impl Group {
    /// 依次让 Bot 发言（调用方拿着 lock）。first 是 [(bot_id, 原因)]：mention / lead / handoff / dm
    pub async fn run(&self, first: Vec<(i64, &'static str)>, turn: Turn) {
        let mut queue = first;
        let mut turns = 0;
        while !queue.is_empty() {
            let (bot_id, why) = queue.remove(0);
            let Some(chat) = bots::chat(self.chat_id) else { continue };
            if !bots::ids_of(&chat).contains(&bot_id) {
                continue;
            }
            if why == "mention" && self.expected.lock().get(&bot_id).copied().unwrap_or(0) > 0 {
                continue;
            }
            if turns >= MAX_TURNS {
                note(self.chat_id, &t!("Bot 们这一轮已经说了 {n} 次，先停一下，等你发话。", n = MAX_TURNS));
                break;
            }
            turns += 1;
            self.handoffs.lock().clear();
            let tn = if why == "dm" { turn.clone() } else { Turn::with_depth(turn.depth()) };
            if let Err(e) = router::runtime(bot_id).respond(self.chat_id, tn).await {
                tracing::error!("群聊 {} 里 Bot {bot_id} 出错：{e:#}", self.chat_id);
                server::broadcast(json!({"type": "error", "chat_id": self.chat_id,
                    "message": t!("{name} 出错了：{e}", name = name_of(bot_id), e = providers::brief(&e))}));
            }
            let hs = self.handoffs.lock().clone();
            let mut next: Vec<(i64, &'static str)> = hs.iter().map(|b| (*b, "handoff")).collect();
            next.extend(queue.into_iter().filter(|q| !hs.contains(&q.0)));
            queue = next;
        }
        self.handoffs.lock().clear();
    }
}

/// 消息里 @ 到的成员，按出现顺序。长名字优先匹配
pub fn mentions(text: &str, member_ids: &[i64]) -> Vec<i64> {
    let low = text.to_lowercase();
    if EVERYONE.iter().any(|w| low.contains(&format!("@{w}"))) {
        return member_ids.to_vec();
    }
    let mut names: Vec<(String, i64)> = member_ids
        .iter()
        .filter_map(|i| bots::get(*i))
        .map(|b| (py::str_of(&b["name"]).to_lowercase(), b["id"].as_i64().unwrap_or(0)))
        .collect();
    names.sort_by_key(|x| std::cmp::Reverse(x.0.chars().count()));
    let lowc: Vec<char> = low.chars().collect();
    let mut taken: Vec<(usize, usize)> = vec![];
    let mut found: Vec<(usize, i64)> = vec![];
    for (name, bid) in names {
        let pat: Vec<char> = format!("@{name}").chars().collect();
        if pat.len() > lowc.len() {
            continue;
        }
        let mut a = 0;
        while a + pat.len() <= lowc.len() {
            if lowc[a..a + pat.len()] == pat[..] {
                let z = a + pat.len();
                if taken.iter().any(|(x, y)| *x < z && a < *y) {
                    a = z;
                    continue;
                }
                let last_ascii = name.chars().last().is_some_and(|c| c.is_ascii());
                if z < lowc.len() && lowc[z].is_ascii() && lowc[z].is_alphanumeric() && last_ascii {
                    a = z;
                    continue;
                }
                taken.push((a, z));
                found.push((a, bid));
                break;
            }
            a += 1;
        }
    }
    found.sort();
    found.into_iter().map(|(_, b)| b).collect()
}

fn message(chat_id: i64, role: &str, content: &str, bot_id: Option<i64>, meta: Value) -> i64 {
    let mid = memory::add(role, content, chat_id, bot_id, &meta);
    server::broadcast(json!({"type": "message", "chat_id": chat_id, "bot_id": bot_id, "message": {
        "id": mid, "ts": py::now(), "role": role, "content": content, "meta": meta, "bot_id": bot_id}}));
    mid
}

pub fn note(chat_id: i64, text: &str) {
    message(chat_id, "task", &format!("[{text}]"), None, json!({"kind": "note"}));
}

pub async fn on_user(chat: &Value, text: &str, via: &str, attachments: &[Value]) {
    let cid = chat["id"].as_i64().unwrap_or(0);
    let mut meta = json!({});
    if !via.is_empty() {
        meta["via"] = json!(via);
    }
    if !attachments.is_empty() {
        meta["attachments"] = json!(attachments);
    }
    message(cid, "user", text, None, meta);
    let g = get(cid);
    let _l = g.lock.lock().await;
    let chat = bots::chat(cid).unwrap_or(chat.clone());
    let ids = bots::ids_of(&chat);
    if ids.is_empty() {
        note(cid, &t!("群里没有 Bot 了，先在右栏添加成员"));
        return;
    }
    let hit = mentions(text, &ids);
    let lead = chat["root_bot"].as_i64().filter(|b| *b != 0).unwrap_or_else(|| {
        let l = chat["lead"].as_i64().unwrap_or(0);
        if ids.contains(&l) { l } else { ids[0] }
    });
    let first: Vec<(i64, &'static str)> = if hit.is_empty() { vec![(lead, "lead")] } else { hit.into_iter().map(|b| (b, "mention")).collect() };
    g.run(first, Turn::default()).await;
}

pub fn handoff(chat_id: i64, from_id: i64, to_id: i64, request: &str) {
    message(chat_id, "task", &t!("→ {name}：{text}", name = name_of(to_id), text = request), Some(from_id),
            json!({"kind": "handoff", "to": to_id, "note": request}));
    let g = get(chat_id);
    let mut h = g.handoffs.lock();
    if !h.contains(&to_id) {
        h.push(to_id);
    }
}

pub fn expect(chat_id: i64, to_id: i64) {
    *get(chat_id).expected.lock().entry(to_id).or_insert(0) += 1;
}

pub fn unexpect(chat_id: i64, to_id: i64) {
    let g = get(chat_id);
    let mut e = g.expected.lock();
    if let Some(n) = e.get_mut(&to_id).filter(|n| **n > 0) {
        *n -= 1;
    }
}

pub async fn after_task(task: Arc<Task>) {
    let (h, status) = {
        let s = task.st.lock();
        (s.handoff.clone(), s.status.clone())
    };
    let Some(h) = h else { return };
    unexpect(task.chat_id, h.to);
    if status != "done" {
        note(task.chat_id, &t!("任务 #{id} 没做完，没有交给 {name}", id = task.id, name = name_of(h.to)));
        return;
    }
    let g = get(task.chat_id);
    let _l = g.lock.lock().await;
    let request = if h.note.is_empty() { t!("资料在上面，请接着做") } else { h.note.clone() };
    message(
        task.chat_id,
        "task",
        &t!("→ {name}：{text}（任务 #{id} 的结果）", name = name_of(h.to), text = request, id = task.id),
        Some(task.bot_id),
        json!({"kind": "handoff", "to": h.to, "note": t!("任务 #{id} 的结果在上面。{text}", id = task.id, text = request), "task_id": task.id}),
    );
    g.run(vec![(h.to, "handoff")], Turn::with_depth(task.depth)).await;
}

pub async fn wake(chat_id: i64, bot_id: i64, turn: Turn) {
    let g = get(chat_id);
    let _l = g.lock.lock().await;
    g.run(vec![(bot_id, "dm")], turn).await;
}
