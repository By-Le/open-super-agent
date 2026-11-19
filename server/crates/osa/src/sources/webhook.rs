//! 外面推过来的事件：通用 Webhook、GitHub。对应 osa/sources/webhook.py。

use super::util::{oneline, short};
use crate::py;
use crate::registry::{HookReq, ParseErr, Parsed, Source};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};

pub const MAX_JSON: usize = 12_000;

fn title_of(data: &Value, text: &str) -> String {
    if let Some(d) = data.as_object() {
        for k in ["title", "subject", "summary", "message", "text", "event", "type", "action", "status", "name"] {
            match d.get(k) {
                Some(Value::String(v)) if !v.trim().is_empty() => return oneline(v, 120),
                Some(Value::Object(v)) => {
                    for kk in ["title", "name", "message"] {
                        if let Some(s) = v.get(kk).and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty()) {
                            return oneline(s, 120);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let t = oneline(text, 120);
    if t.is_empty() { "收到一个 webhook".into() } else { t }
}

fn parse_generic(req: &HookReq, _: &Value) -> Result<Parsed, ParseErr> {
    let text = if !req.json.is_null() {
        py::dumps_indent(&req.json, 1, false)
    } else {
        String::from_utf8_lossy(&req.body).into_owned()
    };
    if text.trim().is_empty() {
        return Ok(Parsed::Skip("请求是空的".into()));
    }
    Ok(Parsed::Events(vec![json!({"title": title_of(&req.json, &text), "text": short(&text, MAX_JSON)})]))
}

// ---------- GitHub ----------
fn sig_ok(secret: &str, body: &[u8], sig: &str) -> bool {
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("hmac");
    m.update(body);
    let want = format!("sha256={}", m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>());
    want.len() == sig.len() && want.bytes().zip(sig.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

fn s(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => py::str_of(other),
    }
}

fn who(d: &Value) -> String {
    let l = &d["sender"]["login"];
    if py::truthy(l) { s(l) } else { String::new() }
}

fn is_bot(d: &Value) -> bool {
    let snd = &d["sender"];
    snd["type"] == "Bot" || s(&snd["login"]).ends_with("[bot]")
}

/// Python 的 d.get(k, "")：没有这个键是默认值，有但是 null 是 "None"
fn g(d: &Value, k: &str, default: &str) -> String {
    match d.get(k) {
        None => default.into(),
        Some(v) => py::str_of(v),
    }
}

/// Python 的 d.get(k) or ""
fn or(d: &Value, k: &str) -> String {
    if py::truthy(&d[k]) { s(&d[k]) } else { String::new() }
}

pub fn gh_event(kind: &str, d: &Value) -> Result<Value, String> {
    let repo = or(&d["repository"], "full_name");
    let who = who(d);
    let action = or(d, "action");
    let pre = if repo.is_empty() { String::new() } else { format!("[{repo}] ") };
    match kind {
        "issues" => {
            let i = &d["issue"];
            let verb = match action.as_str() {
                "opened" => "新 issue",
                "reopened" => "重开 issue",
                "closed" => "关闭 issue",
                "labeled" => "issue 加了标签",
                "assigned" => "issue 指派了人",
                _ => return Err(format!("issue 的 {action}，不处理")),
            };
            let labels: Vec<String> = i["labels"].as_array().map(|a| a.iter().map(|x| g(x, "name", "")).collect()).unwrap_or_default();
            let labels = labels.join("、");
            Ok(json!({"title": format!("{pre}{verb} #{}：{}（{who}）", py::str_of(&i["number"]), g(i, "title", "")), "url": g(i, "html_url", ""),
                "text": format!("{}{}", if labels.is_empty() { String::new() } else { format!("标签：{labels}\n") }, short(&or(i, "body"), 3000))}))
        }
        "issue_comment" => {
            let (i, c) = (&d["issue"], &d["comment"]);
            if action != "created" {
                return Err(format!("评论的 {action}，不处理"));
            }
            let w = if py::truthy(&i["pull_request"]) { "PR" } else { "issue" };
            Ok(json!({"title": format!("{pre}{who} 评论了 {w} #{}：{}", py::str_of(&i["number"]), g(i, "title", "")), "url": g(c, "html_url", ""),
                "text": short(&or(c, "body"), 3000)}))
        }
        "pull_request" => {
            let p = &d["pull_request"];
            let verb = match action.as_str() {
                "closed" => if py::truthy(&p["merged"]) { "合并了 PR" } else { "关闭了 PR" },
                "opened" => "新 PR",
                "reopened" => "重开 PR",
                "ready_for_review" => "PR 可以审了",
                "synchronize" => "PR 有新提交",
                _ => return Err(format!("PR 的 {action}，不处理")),
            };
            let stat = format!("改了 {} 个文件，+{} -{}", g(p, "changed_files", "?"), g(p, "additions", "?"), g(p, "deletions", "?"));
            Ok(json!({"title": format!("{pre}{verb} #{}：{}（{who}）", py::str_of(&p["number"]), g(p, "title", "")), "url": g(p, "html_url", ""),
                "text": format!("{} → {}，{stat}\n{}", g(&p["head"], "ref", ""), g(&p["base"], "ref", ""), short(&or(p, "body"), 3000))}))
        }
        "pull_request_review" => {
            let (rv, p) = (&d["review"], &d["pull_request"]);
            if action != "submitted" {
                return Err(format!("审查的 {action}，不处理"));
            }
            let st = g(rv, "state", "");
            let state = match st.as_str() {
                "approved" => "批准了".to_string(),
                "changes_requested" => "要求修改".into(),
                "commented" => "评论了".into(),
                other => other.to_string(),
            };
            Ok(json!({"title": format!("{pre}{who} {state} PR #{}：{}", py::str_of(&p["number"]), g(p, "title", "")), "url": g(rv, "html_url", ""),
                "text": short(&or(rv, "body"), 3000)}))
        }
        "push" => {
            let commits = d["commits"].as_array().cloned().unwrap_or_default();
            if commits.is_empty() {
                return Err("没有提交的推送（建分支、删分支），不处理".into());
            }
            let r = or(d, "ref");
            let branch = r.strip_prefix("refs/heads/").unwrap_or(&r);
            let lines: Vec<String> = commits.iter().take(20).map(|c| format!("- {} {}（{}）", py::head(&g(c, "id", ""), 7), oneline(&or(c, "message"), 120), g(&c["author"], "name", ""))).collect();
            let more = if commits.len() > 20 { format!("\n……共 {} 个", commits.len()) } else { String::new() };
            Ok(json!({"title": format!("{pre}{who} 推送了 {} 个提交到 {branch}", commits.len()), "url": g(d, "compare", ""), "text": lines.join("\n") + &more}))
        }
        "release" => {
            let rl = &d["release"];
            if !["published", "released", "prereleased"].contains(&action.as_str()) {
                return Err(format!("发布的 {action}，不处理"));
            }
            Ok(json!({"title": format!("{pre}发布了 {}：{}{}", g(rl, "tag_name", ""), or(rl, "name"), if py::truthy(&rl["prerelease"]) { "（预发布）" } else { "" }),
                "url": g(rl, "html_url", ""), "text": short(&or(rl, "body"), 5000)}))
        }
        "workflow_run" => {
            let w = &d["workflow_run"];
            if action != "completed" {
                return Err(format!("Actions 的 {action}，还没跑完，不处理"));
            }
            let concl = or(w, "conclusion");
            let word = if ["failure", "timed_out", "startup_failure"].contains(&concl.as_str()) { "失败".to_string() } else { concl.clone() };
            Ok(json!({"title": format!("{pre}Actions「{}」{word}：{} {}", g(w, "name", ""), g(w, "head_branch", ""), oneline(&or(w, "display_title"), 80)),
                "url": g(w, "html_url", ""), "text": format!("结论：{concl}\n触发：{}，提交 {}", g(w, "event", ""), py::head(&g(w, "head_sha", ""), 7)),
                "conclusion": concl}))
        }
        "discussion" => {
            let ds = &d["discussion"];
            if action != "created" {
                return Err(format!("讨论的 {action}，不处理"));
            }
            Ok(json!({"title": format!("{pre}新讨论 #{}：{}（{who}）", py::str_of(&ds["number"]), g(ds, "title", "")), "url": g(ds, "html_url", ""),
                "text": short(&or(ds, "body"), 3000)}))
        }
        "star" => {
            if action != "created" {
                return Err("取消点星，不处理".into());
            }
            let n = &d["repository"]["stargazers_count"];
            Ok(json!({"title": format!("{pre}{who} 点了星{}", if py::truthy(n) { format!("（现在 {} 颗）", py::str_of(n)) } else { String::new() }),
                "url": g(&d["sender"], "html_url", ""), "text": ""}))
        }
        "fork" => Ok(json!({"title": format!("{pre}{who} fork 了仓库"), "url": g(&d["forkee"], "html_url", ""), "text": ""})),
        _ => Ok(json!({"title": format!("{pre}{kind} {action}").trim().to_string(), "text": short(&py::dumps_indent(d, 1, false), MAX_JSON)})),
    }
}

fn parse_github(req: &HookReq, params: &Value) -> Result<Parsed, ParseErr> {
    let h = &req.headers;
    let kind = h.get("x-github-event").cloned().unwrap_or_default();
    if py::truthy(&params["secret"]) && !sig_ok(&s(&params["secret"]), &req.body, h.get("x-hub-signature-256").map(|x| x.as_str()).unwrap_or("")) {
        return Err(ParseErr::Denied("签名不对：GitHub 里填的 Secret 和这里的不一样".into()));
    }
    if kind.is_empty() {
        return Ok(Parsed::Skip("不像是 GitHub 发来的（没有 X-GitHub-Event）".into()));
    }
    let d = &req.json;
    if !d.is_object() {
        return Ok(Parsed::Skip("GitHub 发来的不是 JSON：在 GitHub 的 webhook 设置里把 Content type 选成 application/json".into()));
    }
    if kind == "ping" {
        let full = or(&d["repository"], "full_name");
        let what = if full.is_empty() { g(d, "hook_id", "") } else { full };
        return Ok(Parsed::Skip(format!("收到 GitHub 的 ping，地址配好了（{what}）")));
    }
    if let Some(evs) = params["events"].as_array().filter(|a| !a.is_empty()) {
        if !evs.iter().any(|e| e == kind.as_str()) {
            return Ok(Parsed::Skip(format!("没选这类事件：{kind}")));
        }
    }
    let skip_bots = params.get("skip_bots").map(py::truthy).unwrap_or(true);
    if skip_bots && is_bot(d) {
        return Ok(Parsed::Skip(format!("机器人（{}）的，不处理", who(d))));
    }
    let mut ev = match gh_event(&kind, d) {
        Ok(e) => e,
        Err(why) => return Ok(Parsed::Skip(why)),
    };
    let only_failed = params.get("only_failed").map(py::truthy).unwrap_or(true);
    let concl = ev["conclusion"].as_str().unwrap_or("").to_string();
    if kind == "workflow_run" && only_failed && ["success", "skipped", "neutral"].contains(&concl.as_str()) {
        return Ok(Parsed::Skip(format!("Actions 成功了（{concl}），不处理")));
    }
    if let Some(o) = ev.as_object_mut() {
        o.remove("conclusion");
    }
    Ok(Parsed::Events(vec![ev]))
}

pub fn sources() -> Vec<Source> {
    let mut w = super::base("webhook");
    w.parse = Some(parse_generic);
    let mut gh = super::base("github");
    gh.parse = Some(parse_github);
    gh.summary = Some(|p| p["events"].as_array().map(|a| a.iter().map(py::str_of).collect::<Vec<_>>().join("、")).unwrap_or_default());
    vec![w, gh]
}
