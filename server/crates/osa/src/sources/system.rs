//! osa 自己的事件：后台任务失败了。对应 osa/sources/system.py。

use super::util::short;
use crate::py;
use crate::registry::Source;
use serde_json::{Value, json};

fn match_task_failed(params: &Value, routine: &Value, kw: &Value) -> Vec<Value> {
    let task = &kw["task"];
    if task["status"] != "failed" {
        return vec![];
    }
    let scope = params["scope"].as_str().unwrap_or("mine");
    if scope == "mine" && task["bot_id"] != routine["bot_id"] {
        return vec![];
    }
    let goal = if py::truthy(&task["goal"]) { py::str_of(&task["goal"]) } else { String::new() };
    let first = goal.lines().next().unwrap_or("");
    let bot = kw["bot"]["name"].as_str().unwrap_or("Bot");
    let result = kw["result"].as_str().unwrap_or("");
    vec![json!({"id": format!("task-{}", py::str_of(&task["id"])),
        "title": format!("{bot} 的任务 #{} 失败了：{}", py::str_of(&task["id"]), short(if goal.is_empty() { "" } else { first }, 80)),
        "text": format!("要做的事：\n{}\n\n失败原因：\n{}", short(&goal, 2000), short(result, 2000))})]
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("task_failed");
    s.matcher = Some(match_task_failed);
    s.summary = Some(|p| if p["scope"] == "all" { "所有 Bot".into() } else { String::new() });
    vec![s]
}
