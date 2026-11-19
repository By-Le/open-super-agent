//! Bot 管理 Bot：查看、新建、修改、删除（Router 的 get_bot / create_bot / update_bot / delete_bot 工具）。
//! 对应 osa/botadmin.py（谁能做什么、确认、结果怎么回到对话，见那里的说明）。

use crate::db;
use crate::dm::{self, Turn, Woke};
use crate::router::{self, Router};
use crate::skills;
use crate::task::{NewTask, Reply, Task};
use crate::{approvals, bots, memories, py, routines};
use regex::Regex;
use rusqlite::params;
use serde_json::{Map, Value, json};
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, LazyLock};

pub const MAX_SKILLS: usize = 10;
pub const MAX_INSTRUCTIONS: usize = 4000;
pub const MAX_SKILL_SHOWN: usize = 3000;
pub const CARD_LIMIT: usize = 12000;

pub const TOOLS: [&str; 4] = ["get_bot", "create_bot", "update_bot", "delete_bot"];

/// 主 Bot 四个都有；别的 Bot 只有 update_bot（只能改自己）。schema 取自 Python 版导出的
pub fn tools_for(bot: &Value) -> Vec<Value> {
    let s = crate::assets::schemas();
    let k = if bot["is_primary"] == true { "botadmin_primary" } else { "botadmin_other" };
    s[k].as_array().cloned().unwrap_or_default()
}

/// 结果写进对话的样子：(kind, 做成了, 没同意, 没同意附了意见, 没做成)
fn text_of(tool: &str) -> (&'static str, &'static str, &'static str, &'static str, &'static str) {
    match tool {
        "create_bot" => (
            "bot_created",
            "[已新建 Bot「{name}」]",
            "[没新建 Bot「{name}」]",
            "[没新建 Bot「{name}」：{note}]",
            "[没建成 Bot「{name}」：{error}]",
        ),
        "update_bot" => (
            "bot_updated",
            "[已修改 Bot「{name}」：{what}]",
            "[没修改 Bot「{name}」]",
            "[没修改 Bot「{name}」：{note}]",
            "[没改成 Bot「{name}」：{error}]",
        ),
        _ => (
            "bot_deleted",
            "[已删除 Bot「{name}」]",
            "[没删除 Bot「{name}」]",
            "[没删除 Bot「{name}」：{note}]",
            "[没删成 Bot「{name}」：{error}]",
        ),
    }
}

fn goal_of(tool: &str) -> &'static str {
    match tool {
        "create_bot" => "新建 Bot",
        "update_bot" => "修改 Bot",
        _ => "删除 Bot",
    }
}

// ---------- 检查参数（不行返回 Err，写给模型看） ----------
fn s(v: &Value) -> String {
    if py::truthy(v) { py::str_of(v) } else { String::new() }
}

fn check_name(name: &Value, exclude: Option<i64>) -> Result<String, String> {
    let name = s(name).trim().trim_start_matches('@').trim().to_string();
    if name.is_empty() {
        return Err("名字是空的".into());
    }
    if py::char_len(&name) > 40 {
        return Err("名字最多 40 个字".into());
    }
    if let Some(b) = bots::by_name(&name)
        && b["id"].as_i64() != exclude
    {
        return Err(format!(
            "已经有叫 {} 的 Bot 了，换个名字{}",
            py::str_of(&b["name"]),
            if exclude.is_none() { "，或者直接用 message_bot 找它" } else { "" }
        ));
    }
    Ok(name)
}

fn check_instructions(text: &Value) -> Result<String, String> {
    let text = s(text).trim().to_string();
    if text.is_empty() {
        return Err("职责是空的：写清楚它负责什么、怎么做".into());
    }
    let n = py::char_len(&text);
    if n > MAX_INSTRUCTIONS {
        return Err(format!("职责太长了（{n} 个字），最多 {MAX_INSTRUCTIONS} 个字，精简一下"));
    }
    Ok(text)
}

fn check_avatar(a: &Value) -> Result<Option<Value>, String> {
    if !a.is_object() || !(py::truthy(&a["shape"]) || py::truthy(&a["color"])) {
        return Ok(None);
    }
    let shape = s(&a["shape"]);
    let color = s(&a["color"]);
    if !shape.is_empty() && !bots::SHAPES.contains(&shape.as_str()) {
        return Err(format!("头像形状只能是 {} 之一", bots::SHAPES.join("、")));
    }
    static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());
    if !color.is_empty() && !HEX.is_match(&color) {
        return Err(format!("头像颜色要写成 #RRGGBB，比如 {}", bots::COLORS[0]));
    }
    let mut o = Map::new();
    if !shape.is_empty() {
        o.insert("shape".into(), json!(shape));
    }
    if !color.is_empty() {
        o.insert("color".into(), json!(color.to_lowercase()));
    }
    Ok(Some(Value::Object(o)))
}

fn check_skills(items: &Value) -> Result<Vec<Value>, String> {
    let items = items.as_array().cloned().unwrap_or_default();
    if items.len() > MAX_SKILLS {
        return Err(format!("技能一次最多 {MAX_SKILLS} 个"));
    }
    let mut out = vec![];
    let mut seen: Vec<String> = vec![];
    for it in items {
        let it = if it.is_object() { it } else { json!({}) };
        let n = skills::check(&s(&it["name"]), &s(&it["body"]))
            .map_err(|e| format!("技能 {}：{e}", if py::truthy(&it["name"]) { py::str_of(&it["name"]) } else { "(没名字)".into() }))?;
        if seen.contains(&n) {
            return Err(format!("技能 /{n} 写了两遍"));
        }
        seen.push(n.clone());
        out.push(json!({"name": n, "description": py::head(&s(&it["description"]), 200), "body": py::str_of(&it["body"])}));
    }
    Ok(out)
}

fn target(name: &Value) -> Result<Value, String> {
    bots::by_name(&s(name)).ok_or_else(|| {
        let others: Vec<String> = bots::list_bots().iter().map(|x| py::str_of(&x["name"])).collect();
        format!("没有叫 {} 的 Bot（没建成或已删除，记忆里说有也不算）。现有：{}", py::str_of(name), others.join("、"))
    })
}

fn own_skills(bot_id: i64) -> Vec<Value> {
    skills::list_for(bot_id).into_iter().filter(|s| s["bot_id"].as_i64() == Some(bot_id)).collect()
}

fn own_skill(bot_id: i64, name: &str) -> Option<Value> {
    own_skills(bot_id).into_iter().find(|s| s["name"] == name)
}

fn skill_line(sk: &Value) -> String {
    let d = sk["description"].as_str().unwrap_or("");
    format!("/{}：{}", py::str_of(&sk["name"]), if d.is_empty() { py::head(sk["body"].as_str().unwrap_or(""), 60) } else { d.to_string() })
}

/// 一次改动的计划：要做的事 + 卡片上给用户看的
struct Plan {
    data: Value,
    label: String,
    shown: Value,
    what: String,
}

fn plan_create(me: &Value, args: &Value) -> Result<Plan, String> {
    if me["is_primary"] != true {
        return Err(format!("只有主 Bot（{}）能新建 Bot，用 message_bot 私信它，把名字和职责写清楚", bots::primary_name()));
    }
    let name = check_name(&args["name"], None)?;
    if bots::list_bots().len() as i64 >= bots::MAX_BOTS {
        return Err(format!("已经有 {} 个 Bot，到上限了，请用户先删掉不用的", bots::MAX_BOTS));
    }
    let ins = check_instructions(&args["instructions"])?;
    let avatar = check_avatar(&args["avatar"])?;
    let sk = check_skills(&args["skills"])?;
    let mut shown = json!({"名字": name, "职责": ins, "技能": sk.iter().map(skill_line).collect::<Vec<_>>()});
    if let Some(a) = &avatar {
        shown["头像"] = a.clone();
    }
    Ok(Plan { data: json!({"name": name, "instructions": ins, "avatar": avatar, "skills": sk}), label: name, shown, what: String::new() })
}

/// 按分句切：分隔符留在前一句的末尾
fn split_sentences(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for c in s.chars() {
        cur.push(c);
        if "。！？；;!?\n".contains(c) {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes()
fn opcodes(a: &[String], b: &[String]) -> Vec<(&'static str, usize, usize, usize, usize)> {
    use std::collections::HashMap;
    let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, x) in b.iter().enumerate() {
        b2j.entry(x.as_str()).or_default().push(j);
    }
    let longest = |alo: usize, ahi: usize, blo: usize, bhi: usize| -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(js) = b2j.get(a[i].as_str()) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).copied().unwrap_or(0) + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi && bestj + bestsize < bhi && a[besti + bestsize] == b[bestj + bestsize] {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    };
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut blocks = vec![];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest(alo, ahi, blo, bhi);
        if k > 0 {
            blocks.push((i, j, k));
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    blocks.sort();
    let mut merged: Vec<(usize, usize, usize)> = vec![];
    for (i, j, k) in blocks {
        if let Some(last) = merged.last_mut()
            && last.0 + last.2 == i
            && last.1 + last.2 == j
        {
            last.2 += k;
            continue;
        }
        merged.push((i, j, k));
    }
    merged.push((a.len(), b.len(), 0));
    let (mut i, mut j) = (0, 0);
    let mut out = vec![];
    for (ai, bj, size) in merged {
        let tag = if i < ai && j < bj {
            "replace"
        } else if i < ai {
            "delete"
        } else if j < bj {
            "insert"
        } else {
            ""
        };
        if !tag.is_empty() {
            out.push((tag, i, ai, j, bj));
        }
        i = ai + size;
        j = bj + size;
        if size > 0 {
            out.push(("equal", ai, i, bj, j));
        }
    }
    out
}

/// 职责的改动，按分句比：[[0, 没变的], [-1, 删掉的], [1, 加上的], …]
fn diff(old: &str, new: &str) -> Value {
    let a = split_sentences(old);
    let b = split_sentences(new);
    let mut out: Vec<(i64, String)> = vec![];
    for (op, i1, i2, j1, j2) in opcodes(&a, &b) {
        let parts: Vec<(i64, &[String])> = if op == "equal" { vec![(0, &a[i1..i2])] } else { vec![(-1, &a[i1..i2]), (1, &b[j1..j2])] };
        for (sign, part) in parts {
            if part.is_empty() {
                continue;
            }
            let text: String = part.concat();
            match out.last_mut() {
                Some(last) if last.0 == sign => last.1 += &text,
                _ => out.push((sign, text)),
            }
        }
    }
    json!(out.into_iter().map(|(s, t)| json!([s, t])).collect::<Vec<_>>())
}

fn plan_update(me: &Value, args: &Value) -> Result<Plan, String> {
    let t = target(&args["name"])?;
    let tid = t["id"].as_i64().unwrap_or(0);
    let tname = py::str_of(&t["name"]);
    if me["is_primary"] != true && tid != me["id"].as_i64().unwrap_or(-1) {
        return Err(format!("你只能改你自己。要改 {tname}，私信主 Bot「{}」，把要改成什么写清楚", bots::primary_name()));
    }
    let mut plan = json!({"id": tid});
    let mut shown = json!({"Bot": tname, "编号": tid});
    let mut what: Vec<String> = vec![];
    let new_name = s(&args["new_name"]).trim().trim_start_matches('@').trim().to_string();
    if !new_name.is_empty() && new_name != tname {
        let n = check_name(&json!(new_name), Some(tid))?;
        plan["name"] = json!(n);
        shown["新名字"] = json!(n);
        what.push(t!("名字改成 {name}", name = new_name));
    }
    let text = s(&args["instructions"]).trim().to_string();
    let old_ins = t["instructions"].as_str().unwrap_or("").to_string();
    if !text.is_empty() && text != old_ins.trim() {
        let ins = check_instructions(&json!(text))?;
        plan["instructions"] = json!(ins);
        shown["职责"] = json!(ins);
        shown["职责改动"] = diff(&old_ins, &text);
        what.push(t!("职责"));
    }
    if let Some(av) = check_avatar(&args["avatar"])? {
        let old = &t["avatar"];
        let new = json!({"shape": if py::truthy(&av["shape"]) { av["shape"].clone() } else { old["shape"].clone() },
                         "color": if py::truthy(&av["color"]) { av["color"].clone() } else { old["color"].clone() }});
        let changed = new != json!({"shape": old["shape"], "color": old["color"]});
        if changed || py::truthy(&old["image"]) {
            plan["avatar"] = new.clone();
            shown["头像"] = new;
            if py::truthy(&old["image"]) {
                shown["不再用图片头像"] = json!(true);
            }
            what.push(t!("头像"));
        }
    }
    let own = own_skills(tid);
    let own_names: Vec<String> = own.iter().map(|x| py::str_of(&x["name"])).collect();
    let add = check_skills(&args["skills"])?;
    let drop: Vec<String> = args["remove_skills"]
        .as_array()
        .map(|a| a.iter().map(s).map(|n| n.trim().trim_start_matches('/').to_string()).filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    for n in &drop {
        if !own_names.contains(n) {
            let mine = own_names.iter().map(|x| format!("/{x}")).collect::<Vec<_>>().join("、");
            return Err(format!(
                "{tname} 没有自己的技能 /{n}（共用的技能不能在这里删）。它自己的技能：{}",
                if mine.is_empty() { "没有".into() } else { mine }
            ));
        }
        if add.iter().any(|s| s["name"] == n.as_str()) {
            return Err(format!("技能 /{n} 又要删又要改，只留一个"));
        }
    }
    if !add.is_empty() || !drop.is_empty() {
        let new_count = add.iter().filter(|s| !own_names.contains(&py::str_of(&s["name"]))).count();
        if own.len() as i64 - drop.len() as i64 + new_count as i64 > skills::MAX_PER_BOT {
            return Err(format!("每个 Bot 最多 {} 个技能", skills::MAX_PER_BOT));
        }
        let mut removes = drop.clone();
        removes.sort();
        removes.dedup();
        plan["skills"] = json!(add);
        plan["remove_skills"] = json!(removes);
        let new: Vec<String> = add.iter().filter(|s| !own_names.contains(&py::str_of(&s["name"]))).map(skill_line).collect();
        if !new.is_empty() {
            shown["新技能"] = json!(new);
        }
        let changed: Vec<String> = add.iter().filter(|s| own_names.contains(&py::str_of(&s["name"]))).map(skill_line).collect();
        if !changed.is_empty() {
            shown["改技能"] = json!(changed);
        }
        if !removes.is_empty() {
            shown["删技能"] = json!(removes.iter().map(|n| format!("/{n}")).collect::<Vec<_>>());
        }
        let names: Vec<String> =
            add.iter().map(|s| py::str_of(&s["name"])).chain(removes.iter().cloned()).map(|n| format!("/{n}")).collect();
        what.push(format!("{} {}", t!("技能"), names.join(" ")));
    }
    if plan.as_object().map(|o| o.len()).unwrap_or(0) == 1 {
        return Err("没有要改的：要改的地方和现在一样，或者都没填。名字、职责、头像、技能至少填一样".into());
    }
    Ok(Plan { data: plan, label: tname, shown, what: what.join(&t!("、")) })
}

fn footprint(b: &Value) -> Value {
    let bid = b["id"].as_i64().unwrap_or(0);
    let n_msgs = b["chat_id"].as_i64().map(|c| db::scalar_i64("select count(*) from messages where chat_id=?", params![c])).unwrap_or(0);
    let in_groups: Vec<Value> = groups_of(bid).iter().map(|c| c["title"].clone()).collect();
    let running = router::tasks().iter().filter(|t| t.bot_id == bid && t.status() == "running").count();
    json!({"会删掉": {"消息": n_msgs, "技能": own_skills(bid).len(), "定时任务和事件": routines::list_for(Some(bid)).len(),
                    "记忆": memories::list_for(bid)["own"].as_array().map(|a| a.len()).unwrap_or(0)},
           "群聊": in_groups, "在做的任务": running})
}

fn plan_delete(me: &Value, args: &Value) -> Result<Plan, String> {
    if me["is_primary"] != true {
        return Err(format!("只有主 Bot（{}）能删除 Bot", bots::primary_name()));
    }
    let t = target(&args["name"])?;
    if t["id"] == me["id"] || t["is_primary"] == true {
        return Err("主 Bot 不能删除。要删的话请用户先在设置里把别的 Bot 设为主 Bot".into());
    }
    let mut shown = json!({"Bot": t["name"], "编号": t["id"]});
    if let Some(f) = footprint(&t).as_object() {
        for (k, v) in f {
            shown[k] = v.clone();
        }
    }
    let reason = s(&args["reason"]).trim().to_string();
    if !reason.is_empty() {
        shown["原因"] = json!(py::head(&reason, 500));
    }
    Ok(Plan { data: json!({"id": t["id"]}), label: py::str_of(&t["name"]), shown, what: String::new() })
}

fn plan(tool: &str, me: &Value, args: &Value) -> Result<Plan, String> {
    match tool {
        "create_bot" => plan_create(me, args),
        "update_bot" => plan_update(me, args),
        _ => plan_delete(me, args),
    }
}

/// 用户能在卡片上直接改的：卡片参数的键 → 工具参数
fn editable(tool: &str) -> &'static [(&'static str, &'static str)] {
    match tool {
        "create_bot" | "update_bot" => &[("职责", "instructions")],
        _ => &[],
    }
}

// ---------- 做 ----------
/// 删掉一个 Bot 和它的一切（网页上删和主 Bot 删都走这里）。主 Bot 不能删
pub async fn remove(bot_id: i64) -> Result<(), String> {
    let Some(b) = bots::get(bot_id) else { return Ok(()) };
    if b["is_primary"] == true {
        return Err(t!("主 Bot 不能删除，先把别的 Bot 设为主 Bot"));
    }
    for t in router::tasks() {
        if t.bot_id == bot_id && t.status() == "running" {
            router::cancel(t.id, &t!("Bot 已删除"));
        }
    }
    approvals::cancel_for_bot(bot_id);
    bots::delete(bot_id)?;
    routines::delete_for_bot(bot_id);
    memories::delete_for_bot(bot_id);
    db::exec("delete from skills where bot_id=?", params![bot_id]);
    router::forget_runtime(bot_id);
    if let Some(c) = b["chat_id"].as_i64() {
        crate::channels::chat_removed(c);
    }
    Ok(())
}

async fn apply(tool: &str, p: &Plan, chat_id: i64, r: &Arc<Router>) -> Result<String, String> {
    let (kind, done, ..) = text_of(tool);
    let d = &p.data;
    let (text, meta) = if tool == "create_bot" {
        let av = if d["avatar"].is_null() { json!(null) } else { d["avatar"].clone() };
        let b = bots::create(d["name"].as_str().unwrap_or(""), &av, d["instructions"].as_str().unwrap_or(""), false)?;
        let bid = b["id"].as_i64().unwrap_or(0);
        for sk in d["skills"].as_array().cloned().unwrap_or_default() {
            skills::save(
                bid,
                sk["name"].as_str().unwrap_or(""),
                sk["body"].as_str().unwrap_or(""),
                sk["description"].as_str().unwrap_or(""),
                "chat",
            )?;
        }
        (t!(done, name = b["name"].clone()), json!({"kind": kind, "new_bot": bid}))
    } else if tool == "update_bot" {
        let bid = d["id"].as_i64().unwrap_or(0);
        for n in d["remove_skills"].as_array().cloned().unwrap_or_default() {
            if let Some(sk) = own_skill(bid, &py::str_of(&n)) {
                skills::delete(sk["id"].as_i64().unwrap_or(0));
            }
        }
        for sk in d["skills"].as_array().cloned().unwrap_or_default() {
            skills::save(
                bid,
                sk["name"].as_str().unwrap_or(""),
                sk["body"].as_str().unwrap_or(""),
                sk["description"].as_str().unwrap_or(""),
                "chat",
            )?;
        }
        let fields = json!({"name": d.get("name").cloned().unwrap_or(Value::Null), "instructions": d.get("instructions").cloned().unwrap_or(Value::Null),
                            "avatar": d.get("avatar").cloned().unwrap_or(Value::Null)});
        let b = bots::update(bid, &fields)?;
        (t!(done, name = b["name"].clone(), what = p.what.clone()), json!({"kind": kind, "bot": b["id"]}))
    } else {
        remove(d["id"].as_i64().unwrap_or(0)).await?;
        (t!(done, name = p.label.clone()), json!({"kind": kind}))
    };
    r.emit(json!({"type": "bots_changed"}), None);
    if py::truthy(&d["skills"]) || py::truthy(&d["remove_skills"]) {
        r.emit(json!({"type": "skills_changed"}), None);
    }
    r.add("task", &text, Some(chat_id), meta);
    Ok(text)
}

/// Router 调的入口。bot：谁在调；chat_id：结果记在哪个对话；private_chat_id：卡片发到哪（它的私聊）。
/// owed、depth：这一轮欠谁回信、私信深度，有了结果叫醒它时带上
#[allow(clippy::too_many_arguments)]
pub async fn call(
    r: &Arc<Router>,
    tool: &str,
    bot: &Value,
    chat_id: i64,
    private_chat_id: i64,
    args: &Value,
    owed: Option<Reply>,
    depth: i64,
) -> String {
    if tool == "get_bot" {
        return view(bot, args);
    }
    let verb = match tool {
        "create_bot" => "建",
        "update_bot" => "改",
        _ => "删",
    };
    let p = match plan(tool, bot, args) {
        Ok(p) => p,
        Err(e) => return format!("没{verb}：{e}"),
    };
    let mut shown = p.shown.clone();
    if let Some(o) = &owed {
        shown["请求方"] = json!(dm::name_of(Some(o.bot)));
    }
    let bot_id = bot["id"].as_i64().unwrap_or(0);
    let bot_name = py::str_of(&bot["name"]);
    let label = p.label.clone();
    let what = match tool {
        "create_bot" => format!("新建 Bot「{label}」"),
        "update_bot" => format!("修改 Bot「{label}」"),
        _ => format!("删除 Bot「{label}」"),
    };
    let after = owed.as_ref().map(|o| format!("，到时你的回复会作为回信发给 {}", dm::name_of(Some(o.bot)))).unwrap_or_default();
    let fake = Task::new(NewTask {
        fake_id: Some(format!("对话·{bot_name}")),
        goal: t!(goal_of(tool)),
        bot_id,
        chat_id,
        approval_chat_id: private_chat_id,
        ..Default::default()
    });
    let r = r.clone();
    let tool = tool.to_string();
    let args = args.clone();
    tokio::spawn(async move {
        let ed = editable(&tool);
        let keys: Vec<&str> = ed.iter().map(|(k, _)| *k).filter(|k| shown.get(*k).is_some()).collect();
        let (ok, note, after_args) =
            approvals::request(&fake, &tool, &shown, approvals::Req { kind: "approval", once: true, limit: CARD_LIMIT, editable: &keys })
                .await;
        if bots::get(bot_id).is_none() {
            return;
        }
        let (_, _, no, no_note, failed) = text_of(&tool);
        let tell = |text: String, edited: Vec<String>| {
            let turn = Turn {
                reply: owed.clone(),
                depth: Arc::new(AtomicI64::new(depth)),
                woke: Some(Woke { kind: "botadmin".into(), from: bot_id, text, edited }),
                started: None,
            };
            dm::wake(bot_id, chat_id, turn)
        };
        if !ok {
            let text = if note.is_empty() { t!(no, name = label.clone()) } else { t!(no_note, name = label.clone(), note = note.clone()) };
            r.add("task", &text, Some(chat_id), json!({"kind": "note"}));
            if !note.is_empty() || owed.is_some() {
                tell(text, vec![]).await;
            }
            return;
        }
        let edited: Vec<String> = keys.iter().filter(|k| after_args[**k] != p.shown[**k]).map(|k| k.to_string()).collect();
        let mut fin = args.clone();
        for (k, a) in ed {
            if edited.iter().any(|e| e == k) {
                fin[*a] = after_args[*k].clone();
            }
        }
        let me = bots::get(bot_id).unwrap_or(Value::Null);
        let text = match plan(&tool, &me, &fin) {
            Ok(fresh) => match apply(&tool, &fresh, chat_id, &r).await {
                Ok(t) => t,
                Err(e) => {
                    let t = t!(failed, name = label.clone(), error = e);
                    r.add("task", &t, Some(chat_id), json!({"kind": "note"}));
                    t
                }
            },
            Err(e) => {
                let t = t!(failed, name = label.clone(), error = e);
                r.add("task", &t, Some(chat_id), json!({"kind": "note"}));
                t
            }
        };
        tell(text, edited).await;
    });
    format!(
        "已弹出确认卡片（{what}），卡片在你的私聊里；用户同意后会自动{verb}好。有了结果你会接着说一轮{after}，之后要做的事（比如私信新 Bot 交代事情）到时再做。现在简短告诉用户确认一下就行，不要说已经{verb}好。"
    )
}

// ---------- 查看 ----------
fn when(ts: &Value) -> String {
    match ts.as_f64().filter(|t| *t != 0.0) {
        Some(t) => py::strftime_at(t, "%Y-%m-%d %H:%M"),
        None => "没聊过".into(),
    }
}

fn chat_stats(b: &Value) -> (i64, Value) {
    let Some(c) = b["chat_id"].as_i64() else { return (0, Value::Null) };
    let r = db::row("select count(*), max(ts) from messages where chat_id=?", params![c]).unwrap_or_default();
    (r.first().and_then(|v| v.as_i64()).unwrap_or(0), r.get(1).cloned().unwrap_or(Value::Null))
}

fn groups_of(bot_id: i64) -> Vec<Value> {
    bots::list_chats().into_iter().filter(|c| c["kind"] == "group" && bots::ids_of(c).contains(&bot_id)).collect()
}

fn running_tasks(bot_id: i64) -> Vec<Arc<Task>> {
    router::tasks().into_iter().filter(|t| t.bot_id == bot_id && t.status() == "running").collect()
}

fn is_event(r: &Value) -> bool {
    routines::is_event(r)
}

fn overview(me: &Value) -> String {
    let rows = bots::list_bots();
    let mut lines = vec![format!("共 {} 个 Bot（最多 {} 个）：", rows.len(), bots::MAX_BOTS)];
    for b in &rows {
        let bid = b["id"].as_i64().unwrap_or(0);
        let tags: Vec<&str> = [if b["is_primary"] == true { "主 Bot" } else { "" }, if b["id"] == me["id"] { "就是你" } else { "" }]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect();
        let (_, last) = chat_stats(b);
        let ins = b["instructions"].as_str().unwrap_or("");
        let role = (if ins.is_empty() { "职责没写明" } else { ins }).replace('\n', " ");
        let rs = routines::list_for(Some(bid));
        let ne = rs.iter().filter(|r| is_event(r)).count();
        let mut stats = vec![format!("技能 {} 个", own_skills(bid).len()), format!("定时任务 {} 个", rs.len() - ne)];
        if ne > 0 {
            stats.push(format!("事件 {ne} 个"));
        }
        let g = groups_of(bid);
        if !g.is_empty() {
            stats.push(format!("在 {} 个群里", g.len()));
        }
        let rt = running_tasks(bid);
        if !rt.is_empty() {
            stats.push(format!("在做 {} 个任务", rt.len()));
        }
        stats.push(format!("最近私聊：{}", when(&last)));
        let tag = if tags.is_empty() { String::new() } else { format!("（{}）", tags.join("，")) };
        lines.push(format!(
            "- {}{tag}：{}{}｜{}",
            py::str_of(&b["name"]),
            py::head(&role, 60),
            if py::char_len(&role) > 60 { "…" } else { "" },
            stats.join("，")
        ));
    }
    lines.push("要看某个 Bot 的完整职责和技能，get_bot 填 name。".into());
    lines.join("\n")
}

fn detail(b: &Value, me: &Value) -> String {
    let bid = b["id"].as_i64().unwrap_or(0);
    let (n, last) = chat_stats(b);
    let av = &b["avatar"];
    let ins = b["instructions"].as_str().unwrap_or("");
    let head = [
        format!(
            "{}（{}{}）",
            py::str_of(&b["name"]),
            if b["is_primary"] == true { "主 Bot" } else { "普通 Bot" },
            if b["id"] == me["id"] { "，就是你" } else { "" }
        ),
        format!(
            "建于 {}；头像：{} {}{}",
            when(&b["created"]),
            py::str_of(&av["shape"]),
            py::str_of(&av["color"]),
            if py::truthy(&av["image"]) { "（用的是一张图片）" } else { "" }
        ),
        format!("私聊：{n} 条消息，最近一条 {}", when(&last)),
        "职责：".into(),
        if ins.is_empty() { "（没写）".into() } else { ins.to_string() },
    ];
    let own = own_skills(bid);
    let own_names: Vec<String> = own.iter().map(|x| py::str_of(&x["name"])).collect();
    let shared: Vec<Value> =
        skills::list_for(bid).into_iter().filter(|s| s["bot_id"].is_null() && !own_names.contains(&py::str_of(&s["name"]))).collect();
    let mut parts =
        vec![head.join("\n"), if own.is_empty() { "它自己的技能：无".to_string() } else { format!("它自己的技能（{} 个）：", own.len()) }];
    let mut budget: i64 = 12000;
    for sk in &own {
        let body_full = sk["body"].as_str().unwrap_or("");
        let blen = py::char_len(body_full);
        let mut body = if blen <= MAX_SKILL_SHOWN {
            body_full.to_string()
        } else {
            format!(
                "{}\n……（后面还有 {} 个字，要全文用 get_bot 的 skill 参数）",
                py::head(body_full, MAX_SKILL_SHOWN),
                blen - MAX_SKILL_SHOWN
            )
        };
        if budget - (py::char_len(&body) as i64) < 0 {
            body = format!("（做法太长没列出来，要看用 get_bot 的 skill 参数：{}）", py::str_of(&sk["name"]));
        }
        budget -= py::char_len(&body) as i64;
        let d = sk["description"].as_str().unwrap_or("");
        parts.push(format!("/{}：{}\n做法：\n{body}", py::str_of(&sk["name"]), if d.is_empty() { "（没写说明）" } else { d }));
    }
    if !shared.is_empty() {
        parts.push(format!(
            "所有 Bot 共用的技能：{}",
            shared.iter().map(|s| format!("/{}", py::str_of(&s["name"]))).collect::<Vec<_>>().join(" ")
        ));
    }
    let rs = routines::list_for(Some(bid));
    for (label, ev) in [("定时任务", false), ("事件", true)] {
        let part: Vec<&Value> = rs.iter().filter(|r| is_event(r) == ev).collect();
        let body = if part.is_empty() {
            "无".to_string()
        } else {
            "\n".to_string()
                + &part
                    .iter()
                    .map(|r| {
                        format!(
                            "- #{} {}（{}{}）：{}",
                            py::str_of(&r["id"]),
                            py::str_of(&r["name"]),
                            routines::describe(&r["trigger"], Some("zh")),
                            if py::truthy(&r["enabled"]) { "" } else { "，已暂停" },
                            py::head(r["prompt"].as_str().unwrap_or(""), 100)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
        };
        parts.push(format!("{label}：{body}"));
    }
    let gs = groups_of(bid);
    let gtext = gs
        .iter()
        .map(|c| format!("「{}」{}", py::str_of(&c["title"]), if c["lead"].as_i64() == Some(bid) { "（协调者）" } else { "" }))
        .collect::<Vec<_>>()
        .join("、");
    parts.push(format!("所在群聊：{}", if gtext.is_empty() { "无".into() } else { gtext }));
    let ts = running_tasks(bid);
    parts.push(format!(
        "正在做的任务：{}",
        if ts.is_empty() {
            "无".into()
        } else {
            "\n".to_string() + &ts.iter().map(|t| format!("- #{} {}", t.id, py::head(&t.goal, 80))).collect::<Vec<_>>().join("\n")
        }
    ));
    parts.push(format!(
        "长期记忆：它自己记了 {} 条（内容只有它自己能看）",
        memories::list_for(bid)["own"].as_array().map(|a| a.len()).unwrap_or(0)
    ));
    parts.join("\n\n")
}

/// get_bot 工具
pub fn view(me: &Value, args: &Value) -> String {
    let name = s(&args["name"]).trim().to_string();
    if name.is_empty() {
        return overview(me);
    }
    let Some(b) = bots::by_name(&name) else {
        let others: Vec<String> = bots::list_bots().iter().map(|x| py::str_of(&x["name"])).collect();
        return format!("没有叫 {name} 的 Bot（没建成或已删除，记忆里说有也不算）。现有：{}", others.join("、"));
    };
    let bid = b["id"].as_i64().unwrap_or(0);
    let sk = s(&args["skill"]).trim().trim_start_matches('/').to_string();
    if !sk.is_empty() {
        let Some(x) = own_skill(bid, &sk).or_else(|| skills::find(bid, &sk)) else {
            let have = skills::list_for(bid).iter().map(|x| format!("/{}", py::str_of(&x["name"]))).collect::<Vec<_>>().join(" ");
            return format!(
                "{} 没有 /{sk} 这个技能。它能用的：{}",
                py::str_of(&b["name"]),
                if have.is_empty() { "无".into() } else { have }
            );
        };
        let whose = if x["bot_id"].is_null() { "所有 Bot 共用的" } else { "它自己的" };
        let d = x["description"].as_str().unwrap_or("");
        return format!(
            "{} 的技能 /{}（{whose}）：{}\n做法：\n{}",
            py::str_of(&b["name"]),
            py::str_of(&x["name"]),
            if d.is_empty() { "（没写说明）" } else { d },
            py::str_of(&x["body"])
        );
    }
    detail(&b, me)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sentence_diff() {
        let d = diff("你好。我是 A。再见。", "你好。我是 B。再见。");
        assert_eq!(d, json!([[0, "你好。"], [-1, "我是 A。"], [1, "我是 B。"], [0, "再见。"]]));
    }
}
