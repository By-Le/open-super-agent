//! 盯一个 GitHub 仓库的动态：新版本、新标签、新提交、新 issue、新 PR。对应 osa/sources/github_repo.py（说明见那里）。

use super::util::{self, FetchErr, oneline, short};
use crate::py;
use crate::registry::Source;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

const API: &str = "https://api.github.com";
const MAX_SEEN: usize = 500;
const MAX_NEW: usize = 20;
static REPO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})/[A-Za-z0-9._-]{1,100}$").unwrap());
static BRANCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^\s~^:?*\[\\]{1,200}$").unwrap());
static PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:git@github\.com:|(?:https?://)?(?:www\.)?github\.com/)").unwrap());

pub fn parse_repo(s: &str) -> Result<(String, String), String> {
    let s = s.trim().split('?').next().unwrap_or("").split('#').next().unwrap_or("").to_string();
    let s = PREFIX.replace(&s, "").into_owned();
    let parts: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
    let repo = parts.iter().take(2).cloned().collect::<Vec<_>>().join("/");
    let repo = repo.strip_suffix(".git").unwrap_or(&repo).to_string();
    let second = repo.split('/').nth(1).unwrap_or("");
    if parts.len() < 2 || !REPO.is_match(&repo) || second == "." || second == ".." {
        return Err("仓库要写成 owner/repo（比如 ollama/ollama），或者直接贴仓库的网址".into());
    }
    let branch = if parts.len() > 3 && (parts[2] == "tree" || parts[2] == "commits") { parts[3..].join("/") } else { String::new() };
    Ok((repo, branch))
}

fn validate(mut p: Value) -> Result<Value, String> {
    let (repo, branch) = parse_repo(&if py::truthy(&p["repo"]) { py::str_of(&p["repo"]) } else { String::new() })?;
    p["repo"] = json!(repo);
    if !py::truthy(&p["what"]) {
        p["what"] = json!("releases");
    }
    if p["what"] == "commits" && !py::truthy(&p["branch"]) && !branch.is_empty() {
        p["branch"] = json!(branch);
    }
    if py::truthy(&p["branch"]) && !BRANCH.is_match(&py::str_of(&p["branch"])) {
        return Err("分支名里不能有空格和 ~ ^ : ? * [ \\ 这些字符".into());
    }
    Ok(p)
}

fn error_of(r: &util::Resp, p: &Value) -> String {
    let code = r.status;
    let repo = py::str_of(&p["repo"]);
    let msg = r.json()["message"].as_str().unwrap_or("").to_string();
    let token = py::truthy(&p["token"]);
    if (code == 403 || code == 429) && (r.header("x-ratelimit-remaining") == "0" || msg.to_lowercase().contains("rate limit")) {
        let reset = r.header("x-ratelimit-reset");
        let when = match reset.parse::<i64>() {
            Ok(t) if !reset.is_empty() && reset.chars().all(|c| c.is_ascii_digit()) => {
                format!("{} 之后", py::strftime_at(t as f64, "%H:%M"))
            }
            _ => "过一会儿".into(),
        };
        if token {
            return format!("GitHub 的调用次数用完了，{when}恢复");
        }
        return format!(
            "GitHub 的调用次数用完了：不填 token 每个 IP 每小时只能调 60 次（这台机器上别的程序也算），{when}恢复。填一个 token 能到每小时 5000 次"
        );
    }
    if code == 401 {
        return "GitHub 不认这个 token：填错了或者过期了".into();
    }
    if (code == 404 || code == 422) && p["what"] == "commits" && py::truthy(&p["branch"]) {
        return format!("找不到 {repo} 的分支 {}：仓库名或分支名写错了，或者是私有仓库（要填有权限的 token）", py::str_of(&p["branch"]));
    }
    if code == 404 {
        return format!("找不到仓库 {repo}：名字写错了，或者是私有仓库（要填有权限的 token）");
    }
    if code == 403 {
        return format!(
            "GitHub 拒绝了：{}{}",
            if msg.is_empty() { "HTTP 403".into() } else { msg },
            if token { "（token 可能没有这个仓库的权限）" } else { "" }
        );
    }
    if code >= 500 {
        return format!("GitHub 那边出错了（HTTP {code}），下次再看");
    }
    format!("GitHub 返回了错误：HTTP {code}{}", if msg.is_empty() { String::new() } else { format!(" {msg}") })
}

async fn api(path: &str, query: &[(&str, String)], p: &Value) -> Result<Vec<Value>, String> {
    let mut h = vec![("Accept", "application/vnd.github+json".to_string()), ("X-GitHub-Api-Version", "2022-11-28".to_string())];
    if py::truthy(&p["token"]) {
        h.push(("Authorization", format!("Bearer {}", py::str_of(&p["token"]))));
    }
    match util::get(&format!("{API}{path}"), 20.0, 5_000_000, &h, query).await {
        Ok(r) => Ok(r.json().as_array().cloned().unwrap_or_default()),
        Err(FetchErr::Status(r)) if r.status == 409 => Ok(vec![]),
        Err(FetchErr::Status(r)) => Err(error_of(&r, p)),
        Err(FetchErr::Timeout) => Err("连 GitHub 超时了，下次再看".into()),
        Err(FetchErr::Request(s)) => Err(format!("连不上 GitHub：{}", if s.is_empty() { "RequestError".into() } else { oneline(&s, 150) })),
        Err(FetchErr::Value(s)) => Err(s),
    }
}

fn t(iso: &Value) -> String {
    let s = if py::truthy(iso) { py::str_of(iso) } else { return String::new() };
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn login(d: &Value) -> String {
    if d.is_object() && py::truthy(&d["login"]) { py::str_of(&d["login"]) } else { String::new() }
}

fn meta(pairs: &[(&str, String)]) -> String {
    pairs.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}：{v}")).collect::<Vec<_>>().join("　")
}

fn or(v: &Value, k: &str) -> String {
    if py::truthy(&v[k]) { py::str_of(&v[k]) } else { String::new() }
}

fn quote(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20").replace("%2F", "/")
}

async fn items(p: &Value) -> Result<Vec<(String, Value)>, String> {
    let repo = py::str_of(&p["repo"]);
    let what = if py::truthy(&p["what"]) { py::str_of(&p["what"]) } else { "releases".into() };
    let mut out = vec![];
    match what.as_str() {
        "releases" => {
            for rl in api(&format!("/repos/{repo}/releases"), &[("per_page", "30".into())], p).await? {
                if !rl.is_object() || py::truthy(&rl["draft"]) {
                    continue;
                }
                let tag = or(&rl, "tag_name");
                let name = or(&rl, "name").trim().to_string();
                let head =
                    format!("{tag}{}", if !name.is_empty() && name != tag { format!("：{}", oneline(&name, 100)) } else { String::new() });
                let body = {
                    let b = short(&or(&rl, "body"), 4000);
                    if b.is_empty() { "（没写发布说明）".into() } else { b }
                };
                let when = if py::truthy(&rl["published_at"]) { &rl["published_at"] } else { &rl["created_at"] };
                out.push((
                    format!("r{}", py::str_of(&rl["id"])),
                    json!({
                    "title": format!("[{repo}] 发布了 {head}{}", if py::truthy(&rl["prerelease"]) { "（预发布）" } else { "" }),
                    "url": if py::truthy(&rl["html_url"]) { or(&rl, "html_url") } else { format!("https://github.com/{repo}/releases") },
                    "text": meta(&[("发布时间", t(when)), ("发布人", login(&rl["author"]))]) + "\n" + &body}),
                ));
            }
        }
        "tags" => {
            for tg in api(&format!("/repos/{repo}/tags"), &[("per_page", "100".into())], p).await? {
                let name = if tg.is_object() { or(&tg, "name") } else { String::new() };
                if name.is_empty() {
                    continue;
                }
                let sha = py::head(&or(&tg["commit"], "sha"), 7);
                out.push((format!("t{name}"), json!({"title": format!("[{repo}] 打了新标签 {name}"), "url": format!("https://github.com/{repo}/releases/tag/{}", quote(&name)),
                    "text": format!("指向的提交：{sha}\n这个标签之前的提交：https://github.com/{repo}/commits/{}", quote(&name))})));
            }
        }
        "commits" => {
            let mut q = vec![("per_page", "30".to_string())];
            if py::truthy(&p["branch"]) {
                q.push(("sha", py::str_of(&p["branch"])));
            }
            for c in api(&format!("/repos/{repo}/commits"), &q, p).await? {
                if !c.is_object() || !py::truthy(&c["sha"]) {
                    continue;
                }
                let sha = py::str_of(&c["sha"]);
                let cm = &c["commit"];
                let msg = or(cm, "message").trim().to_string();
                let lines: Vec<&str> = if msg.is_empty() { vec![""] } else { msg.lines().collect() };
                let who = {
                    let l = login(&c["author"]);
                    if l.is_empty() { or(&cm["author"], "name") } else { l }
                };
                let rest = if lines.len() > 1 { format!("\n{}", short(lines[1..].join("\n").trim(), 1500)) } else { String::new() };
                out.push((format!("c{}", py::head(&sha, 12)), json!({
                    "title": format!("[{repo}] 新提交 {}：{}{}", py::head(&sha, 7), oneline(lines[0], 120), if who.is_empty() { String::new() } else { format!("（{who}）") }),
                    "url": if py::truthy(&c["html_url"]) { or(&c, "html_url") } else { format!("https://github.com/{repo}/commit/{sha}") },
                    "text": meta(&[("时间", t(&cm["author"]["date"])), ("分支", or(p, "branch"))]) + &rest})));
            }
        }
        "issues" => {
            let q = [("state", "all".to_string()), ("sort", "created".into()), ("direction", "desc".into()), ("per_page", "30".into())];
            for i in api(&format!("/repos/{repo}/issues"), &q, p).await? {
                if !i.is_object() || py::truthy(&i["pull_request"]) {
                    continue;
                }
                let labels = i["labels"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|x| x.is_object())
                            .map(|x| x["name"].as_str().unwrap_or("").to_string())
                            .collect::<Vec<_>>()
                            .join("、")
                    })
                    .unwrap_or_default();
                out.push((format!("i{}", py::str_of(&i["number"])), json!({
                    "title": format!("[{repo}] 新 issue #{}：{}（{}）{}", py::str_of(&i["number"]), oneline(&or(&i, "title"), 150), login(&i["user"]), if i["state"] == "closed" { "（已关闭）" } else { "" }),
                    "url": or(&i, "html_url"),
                    "text": meta(&[("时间", t(&i["created_at"])), ("标签", labels)]) + "\n" + &short(&or(&i, "body"), 2000)})));
            }
        }
        "pulls" => {
            let q = [("state", "all".to_string()), ("sort", "created".into()), ("direction", "desc".into()), ("per_page", "30".into())];
            for pr in api(&format!("/repos/{repo}/pulls"), &q, p).await? {
                if !pr.is_object() {
                    continue;
                }
                let state = if py::truthy(&pr["merged_at"]) {
                    "（已合并）"
                } else if pr["state"] == "closed" {
                    "（已关闭）"
                } else if py::truthy(&pr["draft"]) {
                    "（草稿）"
                } else {
                    ""
                };
                let branches = format!("{} → {}", pr["head"]["label"].as_str().unwrap_or(""), pr["base"]["ref"].as_str().unwrap_or(""));
                out.push((format!("p{}", py::str_of(&pr["number"])), json!({
                    "title": format!("[{repo}] 新 PR #{}：{}（{}）{state}", py::str_of(&pr["number"]), oneline(&or(&pr, "title"), 150), login(&pr["user"])),
                    "url": or(&pr, "html_url"),
                    "text": meta(&[("时间", t(&pr["created_at"])), ("分支", branches)]) + "\n" + &short(&or(&pr, "body"), 2000)})));
            }
        }
        other => return Err(format!("不认识要看的东西：{other}")),
    }
    Ok(out)
}

async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let what = if py::truthy(&params["what"]) { py::str_of(&params["what"]) } else { "releases".into() };
    let its = items(&params).await?;
    let key = format!("{}|{what}|{}", py::str_of(&params["repo"]), or(&params, "branch"));
    let same = state.is_object() && state["key"].as_str() == Some(key.as_str());
    let seen: Vec<String> =
        if same { state["seen"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default() } else { vec![] };
    let mut all = vec![];
    for k in its.iter().map(|(k, _)| k.clone()).chain(seen.iter().cloned()) {
        if !all.contains(&k) {
            all.push(k);
        }
    }
    all.truncate(MAX_SEEN);
    let new_state = json!({"key": key, "seen": all});
    if state.is_null() {
        return Ok((its.first().map(|(_, e)| vec![e.clone()]).unwrap_or_default(), new_state));
    }
    if !same {
        return Ok((vec![], new_state));
    }
    let mut old: std::collections::HashSet<String> = seen.into_iter().collect();
    let mut fresh = vec![];
    for (k, mut ev) in its {
        if old.insert(k.clone()) {
            ev["id"] = json!(k);
            fresh.push(ev);
        }
    }
    let n = fresh.len();
    let mut events: Vec<Value> = fresh.into_iter().take(MAX_NEW).collect();
    if n > MAX_NEW {
        let t = format!("{}\n（这次一共有 {n} 条新的，只列了最新的 {MAX_NEW} 条）", events[0]["text"].as_str().unwrap_or(""));
        events[0]["text"] = json!(t);
    }
    Ok((events, new_state))
}

fn summary(p: &Value) -> String {
    let repo = or(p, "repo");
    let what = if py::truthy(&p["what"]) { py::str_of(&p["what"]) } else { "releases".into() };
    if what == "commits" {
        return if py::truthy(&p["branch"]) {
            format!("{repo}@{} · commits", py::str_of(&p["branch"]))
        } else {
            format!("{repo} · commits")
        };
    }
    format!("{repo} · {what}")
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("github_repo");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.validate = Some(validate);
    s.summary = Some(summary);
    vec![s]
}
