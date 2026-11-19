//! 长期记忆：每个 Bot 自己记得的事，加上所有 Bot 共享的用户事实（bot_id 为空）。kind=work 是自动记的“做过的事”。
//! 对应 osa/memories.py。

use crate::db::{self, RowExt};
use crate::{memory, providers, py};
use rusqlite::params;
use serde_json::{Value, json};

pub const MAX_LEN: usize = 500;
pub const MAX_NOTES: i64 = 200;
pub const MAX_SHARED: i64 = 50;
pub const KEEP_WORK: i64 = 20;
pub const PROMPT_NOTES: i64 = 80;
pub const PROMPT_RECENT: i64 = 60;
pub const PROMPT_RELATED: usize = 20;
pub const PROMPT_WORK: i64 = 10;

const COLS: &str = "id, bot_id, kind, text, source, created, updated";

pub fn migrate() {
    db::batch(
        "create table if not exists memories(
          id integer primary key, bot_id integer, kind text default 'note', text text, source text default 'chat',
          created real, updated real);
         create index if not exists memories_bot on memories(bot_id, kind);",
    );
}

fn out(r: &[Value]) -> Value {
    json!({"id": r[0], "bot_id": r[1], "shared": r[1].is_null(), "kind": r[2], "text": r[3], "source": r[4],
           "created": r[5], "updated": r[6]})
}

pub fn get(mid: i64) -> Option<Value> {
    db::row(&format!("select {COLS} from memories where id=?"), params![mid]).map(|r| out(&r))
}

fn list(bot_id: Option<i64>, kind: &str, limit: i64) -> Vec<Value> {
    db::rows(
        &format!("select {COLS} from memories where bot_id is ? and kind=? order by updated desc limit ?"),
        params![bot_id, kind, limit],
    )
    .iter()
    .map(|r| out(r))
    .collect()
}

pub fn list_for(bot_id: i64) -> Value {
    json!({"shared": list(None, "note", 1000), "own": list(Some(bot_id), "note", 1000), "work": list(Some(bot_id), "work", 1000)})
}

fn clean(text: &str) -> Result<String, String> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return Err(t!("记忆内容不能为空"));
    }
    Ok(py::head(&t, MAX_LEN))
}

pub fn add(bot_id: Option<i64>, text: &str, source: &str) -> Result<Value, String> {
    let text = clean(text)?;
    let now = py::now();
    if let Some(old) = db::row("select id from memories where bot_id is ? and kind='note' and lower(text)=lower(?)", params![bot_id, text])
    {
        db::exec("update memories set updated=? where id=?", params![now, old.i(0)]);
        return Ok(get(old.i(0)).unwrap_or(Value::Null));
    }
    let n = db::scalar_i64("select count(*) from memories where bot_id is ? and kind='note'", params![bot_id]);
    let cap = if bot_id.is_none() { MAX_SHARED } else { MAX_NOTES };
    if n >= cap {
        return Err(if bot_id.is_none() {
            t!("共享的用户事实最多 {n} 条，先删掉一些旧的", n = cap)
        } else {
            t!("这个 Bot 的记忆最多 {n} 条，先删掉一些旧的", n = cap)
        });
    }
    let mid = db::insert(
        "insert into memories(bot_id, kind, text, source, created, updated) values(?,?,?,?,?,?)",
        params![bot_id, "note", text, source, now, now],
    );
    Ok(get(mid).unwrap_or(Value::Null))
}

/// 改内容，或者在“共享 / 只给这个 Bot”之间移动。bot_id：None 不改，Some(None) 改成共享
pub fn update(mid: i64, text: Option<&str>, bot_id: Option<Option<i64>>) -> Result<Value, String> {
    let m = get(mid).ok_or_else(|| t!("没有 #{id} 这条记忆", id = mid))?;
    let now = py::now();
    if let Some(text) = text {
        let t = clean(text)?;
        db::exec("update memories set updated=?, text=? where id=?", params![now, t, mid]);
    } else {
        db::exec("update memories set updated=? where id=?", params![now, mid]);
    }
    if let Some(b) = bot_id
        && m["kind"] == "note"
    {
        db::exec("update memories set bot_id=? where id=?", params![b, mid]);
    }
    Ok(get(mid).unwrap_or(Value::Null))
}

pub fn delete(mid: i64) {
    db::exec("delete from memories where id=?", params![mid]);
}

pub fn delete_for_bot(bot_id: i64) {
    db::exec("delete from memories where bot_id=?", params![bot_id]);
}

pub fn visible(mid: i64, bot_id: i64) -> bool {
    match get(mid) {
        Some(m) => m["bot_id"].is_null() || m["bot_id"].as_i64() == Some(bot_id),
        None => false,
    }
}

pub fn log_work(bot_id: i64, task_id: i64, goal: &str) {
    let line = goal.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
    let line = py::head(line, 100);
    if line.is_empty() {
        return;
    }
    let now = py::now();
    db::exec(
        "insert into memories(bot_id, kind, text, source, created, updated) values(?,?,?,?,?,?)",
        params![
            bot_id,
            "work",
            t!("{date} 任务 #{id}：{goal}", date = py::strftime_at(now, "%m-%d"), id = task_id, goal = line),
            "task",
            now,
            now
        ],
    );
    db::exec(
        "delete from memories where bot_id=? and kind='work' and id not in
        (select id from memories where bot_id=? and kind='work' order by id desc limit ?)",
        params![bot_id, bot_id, KEEP_WORK],
    );
}

fn lines(items: &[Value]) -> String {
    items.iter().map(|m| format!("- #{} {}", py::str_of(&m["id"]), py::str_of(&m["text"]))).collect::<Vec<_>>().join("\n")
}

fn count(bot_id: Option<i64>) -> i64 {
    db::scalar_i64("select count(*) from memories where bot_id is ? and kind='note'", params![bot_id])
}

pub async fn related(bot_id: i64, text: &str) -> Vec<i64> {
    if text.is_empty() || count(None).max(count(Some(bot_id))) <= PROMPT_NOTES {
        return vec![];
    }
    crate::search::search(text, Some(bot_id), &["memory"], PROMPT_RELATED * 2, &["note"])
        .await
        .into_iter()
        .filter_map(|h| h["ref"].as_i64())
        .collect()
}

fn shown(bot_id: i64) -> (bool, Vec<Value>, Vec<Value>) {
    let full = count(None).max(count(Some(bot_id))) <= PROMPT_NOTES;
    let n = if full { PROMPT_NOTES } else { PROMPT_RECENT };
    (full, list(None, "note", n), list(Some(bot_id), "note", n))
}

pub fn prompt_block(bot_id: i64) -> String {
    let (full, shared, own) = shown(bot_id);
    let s = lines(&shared);
    let o = lines(&own);
    let mut parts = vec![
        format!("关于用户（所有 Bot 共享）：\n{}", if s.is_empty() { "（暂无）" } else { &s }),
        format!("你自己记得的：\n{}", if o.is_empty() { "（暂无）" } else { &o }),
    ];
    if !full {
        parts
            .push("（记忆比较多，这里只放了最近的；和这次对话可能相关的更早记忆附在最新一条消息后面，其余的用 search_memory 查。）".into());
    }
    parts.join("\n")
}

pub fn turn_block(bot_id: i64, related_ids: &[i64]) -> String {
    let (_, shared, own) = shown(bot_id);
    let shown_ids: Vec<i64> = shared.iter().chain(own.iter()).filter_map(|m| m["id"].as_i64()).collect();
    let extra: Vec<Value> = related_ids
        .iter()
        .filter(|i| !shown_ids.contains(i))
        .filter_map(|i| get(*i))
        .filter(|m| m["kind"] == "note")
        .take(PROMPT_RELATED)
        .collect();
    let work = list(Some(bot_id), "work", PROMPT_WORK);
    let mut parts = vec![];
    if !extra.is_empty() {
        parts.push(format!("和这次对话可能相关的更早记忆（#后面是编号）：\n{}", lines(&extra)));
    }
    if !work.is_empty() {
        parts.push(format!("最近做过的事（自动记录）：\n{}", lines(&work)));
    }
    parts.join("\n\n")
}

pub fn worker_block(bot_id: i64) -> String {
    let mut items = list(None, "note", PROMPT_NOTES);
    items.extend(list(Some(bot_id), "note", PROMPT_NOTES));
    items.iter().map(|m| format!("- {}", py::str_of(&m["text"]))).collect::<Vec<_>>().join("\n")
}

const IMPORT_PROMPT: &str = "用户从别的 AI 助手（ChatGPT、Claude、Gemini 等）导出了它记得的关于用户的内容，请整理成一条条独立的长期记忆。
- 每条是一句完整、独立能看懂的话，用中文，以“用户”开头或直接陈述。
- 合并重复的，去掉一次性的、过时的、和用户无关的内容；“已有的记忆”里已经说过的也去掉。
- 关于用户本人的基本事实（名字、时区、所在地、常用语言、职业）shared=true，其余（偏好、习惯、正在做的事）shared=false。
只输出 JSON：{\"items\": [{\"text\": \"...\", \"shared\": false}]}";

fn import_prompt() -> String {
    if crate::i18n::lang() == "zh" {
        IMPORT_PROMPT.into()
    } else {
        IMPORT_PROMPT.replace("用中文，以“用户”开头", "用英文，以“The user”开头")
    }
}

fn strip_bullet(l: &str) -> &str {
    l.trim_matches(|c: char| " -•*\t".contains(c))
}

pub async fn parse_import(text: &str, existing: &[String]) -> Result<Vec<Value>, String> {
    let text = py::head(text.trim(), 20000);
    if text.is_empty() {
        return Err(t!("没有可导入的内容"));
    }
    let have = if existing.is_empty() {
        "（无）".to_string()
    } else {
        existing.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n")
    };
    let msgs = json!([
        {"role": "system", "content": import_prompt()},
        {"role": "user", "content": format!("已有的记忆：\n{have}\n\n导出的内容：\n{text}")}
    ]);
    if let Ok(resp) = providers::complete_role("filter", &msgs, Some(json!({"type": "json_object"}))).await
        && let Ok(v) = serde_json::from_str::<Value>(&resp.content)
    {
        let items = v["items"].as_array().cloned().unwrap_or_default();
        let out: Vec<Value> = items
            .iter()
            .filter(|i| i.is_object() && !py::str_of(&i["text"]).trim().is_empty() && !i["text"].is_null())
            .map(|i| json!({"text": py::str_of(&i["text"]).trim(), "shared": py::truthy(&i["shared"])}))
            .collect();
        if !out.is_empty() {
            return Ok(out);
        }
    }
    Ok(text.lines().filter(|l| !strip_bullet(l).is_empty()).map(|l| json!({"text": strip_bullet(l), "shared": false})).collect())
}

pub async fn import_text(bot_id: i64, text: &str) -> Result<Vec<Value>, String> {
    let mut known: Vec<String> = list(None, "note", 1000).iter().map(|m| py::str_of(&m["text"])).collect();
    known.extend(list(Some(bot_id), "note", 1000).iter().map(|m| py::str_of(&m["text"])));
    let items = memory::WHO.scope(memory::Who { bot_id: Some(bot_id), ..Default::default() }, parse_import(text, &known)).await?;
    let mut saved = vec![];
    for item in items {
        let owner = if py::truthy(&item["shared"]) { None } else { Some(bot_id) };
        if let Ok(m) = add(owner, &py::str_of(&item["text"]), "import") {
            saved.push(m);
        }
    }
    Ok(saved)
}
