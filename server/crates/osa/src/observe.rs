//! 观测：Bot 们花了多少、做了多少、快不快、哪里出了问题（侧栏“观测”页）。对应 osa/observe.py（说明见那里）。
//! 时间按看的人的时区分桶：前端传 tz（Date.getTimezoneOffset()，分钟，东八区是 -480）。

use crate::db::{self, J, RowExt};
use crate::web::{Q, Res, ok, qi, qs};
use crate::{bots, config, py, tasklog};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub const DAY: f64 = 86400.0;
pub const WEEK: f64 = 7.0 * 86400.0;
pub const TOP: usize = 30;
pub const UNKNOWN: i64 = -1;
static ROUTINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[任务已派发\] \[(?:定时任务|事件) (.+?)\] ").unwrap());
static RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[任务 #\d+ 结果\]\n?").unwrap());

pub fn range_days(rng: &str) -> Option<Option<i64>> {
    match rng {
        "today" => Some(Some(1)),
        "7d" => Some(Some(7)),
        "30d" => Some(Some(30)),
        "all" => Some(None),
        _ => None,
    }
}

fn bot_sql(bot: Option<i64>, col: &str) -> (String, Vec<Value>) {
    match bot {
        None => (String::new(), vec![]),
        Some(UNKNOWN) => (format!(" and {col} is null"), vec![]),
        Some(b) => (format!(" and {col}=?"), vec![json!(b)]),
    }
}

fn q(sql: &str, args: &[Value]) -> Vec<Vec<Value>> {
    db::rows(sql, rusqlite::params_from_iter(args.iter().map(J)))
}

fn args(head: &[Value], tail: &[Value]) -> Vec<Value> {
    head.iter().chain(tail.iter()).cloned().collect()
}

pub struct Window {
    pub start: f64,
    pub end: f64,
    pub size: f64,
    pub prev: Option<(f64, f64)>,
}

fn floor_div(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

pub fn window(rng: &str, tz: i64, now: Option<f64>) -> Window {
    let now = now.unwrap_or_else(py::now);
    let off = (tz * 60) as f64;
    let today0 = floor_div(now - off, DAY) * DAY + off;
    let days = range_days(rng).unwrap_or(Some(7));
    match days {
        None => {
            let a = db::scalar("select min(ts) from llm_calls", []).as_f64().filter(|x| *x != 0.0);
            let b = db::scalar(&format!("select min(ts) from messages where {}", tasklog::DISPATCH), []).as_f64().filter(|x| *x != 0.0);
            let first = [a, b].into_iter().flatten().fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.min(x)))).unwrap_or(today0);
            let mut start = floor_div(first - off, DAY) * DAY + off;
            let size = if (today0 - start) / DAY + 1.0 > 120.0 {
                start -= (floor_div(start - off, DAY) + 3.0).rem_euclid(7.0) * DAY;
                WEEK
            } else {
                DAY
            };
            Window { start, end: today0 + DAY, size, prev: None }
        }
        Some(days) => {
            let start = today0 - (days - 1) as f64 * DAY;
            Window { start, end: today0 + DAY, size: if days == 1 { 3600.0 } else { DAY }, prev: Some((start - days as f64 * DAY, start)) }
        }
    }
}

const U: &str = "coalesce(sum(cost),0), coalesce(sum(cache_hit+cache_miss+output),0), coalesce(sum(cache_hit),0),
    coalesce(sum(cache_miss),0), coalesce(sum(output),0), sum(case when err is null then 1 else 0 end),
    sum(case when err is not null then 1 else 0 end)";

fn round_opt(v: &Value) -> Value {
    match v.as_f64() {
        Some(f) => json!(py::round0(f)),
        None => Value::Null,
    }
}

fn or0(v: &Value) -> Value {
    if v.is_null() { json!(0) } else { v.clone() }
}

fn rate(hit: i64, miss: i64) -> Value {
    if hit + miss > 0 { json!(py::round_n(hit as f64 / (hit + miss) as f64, 3)) } else { json!(0) }
}

fn usage_kpi(a: f64, b: f64, bot: Option<i64>) -> Value {
    let (w, ba) = bot_sql(bot, "bot_id");
    let r = q(&format!("select {U} from llm_calls where ts>=? and ts<?{w}"), &args(&[json!(a), json!(b)], &ba))
        .into_iter()
        .next()
        .unwrap_or_default();
    let ttft = q(
        &format!("select avg(ttft) from llm_calls where ts>=? and ts<? and role='router' and err is null and ttft is not null{w}"),
        &args(&[json!(a), json!(b)], &ba),
    )
    .into_iter()
    .next()
    .and_then(|r| r.into_iter().next())
    .unwrap_or(Value::Null);
    let (hit, miss) = (r.i(2), r.i(3));
    json!({"cost": py::round_n(r.f(0), 4), "tokens": r[1], "input": hit + miss, "output": r[4], "cache_hit": hit,
           "cache_rate": rate(hit, miss), "calls": or0(&r[5]), "errors": or0(&r[6]), "ttft": round_opt(&ttft),
           "speed": speed(&format!("ts>=? and ts<?{w}"), &args(&[json!(a), json!(b)], &ba))})
}

fn speed(wh: &str, a: &[Value]) -> Value {
    let r = q(
        &format!(
            "select sum(output), sum(ms-ttft) from llm_calls where {wh} and err is null and output>0 and ttft is not null and ms>ttft"
        ),
        a,
    )
    .into_iter()
    .next()
    .unwrap_or_default();
    match (r.first().and_then(|v| v.as_f64()), r.get(1).and_then(|v| v.as_f64())) {
        (Some(o), Some(d)) if d != 0.0 => json!(py::round_n(o / (d / 1000.0), 1)),
        _ => Value::Null,
    }
}

struct Groups {
    series: Vec<Vec<Value>>,
    by_bot: Vec<Vec<Value>>,
    models: Vec<Vec<Value>>,
    roles: Vec<Vec<Value>>,
    hours: Vec<Vec<Value>>,
}

fn usage_groups(w: &Window, tz: i64, bot: Option<i64>) -> Groups {
    let (a, b, size) = (w.start, w.end, w.size);
    let (ws, ba) = bot_sql(bot, "bot_id");
    let base = format!("from llm_calls where ts>=? and ts<?{ws}");
    let ab = args(&[json!(a), json!(b)], &ba);
    let series = q(
        &format!("select cast((ts-?)/? as integer), coalesce(bot_id,{UNKNOWN}), {U} {base} group by 1, 2"),
        &args(&[json!(a), json!(size)], &ab),
    );
    let by_bot = q(
        &format!("select coalesce(bot_id,{UNKNOWN}), {U}, avg(case when role='router' and err is null then ttft end) {base} group by 1"),
        &ab,
    );
    let models = q(
        &format!(
            "select coalesce(provider,''), model, {U}, avg(case when err is null then ttft end), max(ts) {base} group by 1, 2 order by 1 desc"
        ),
        &ab,
    );
    let roles = q(&format!("select role, {U} {base} group by 1 order by 2 desc"), &ab);
    let off = (tz * 60) as f64;
    let hours = q(
        &format!("select (cast((ts-?)/86400 as integer)+3)%7, (cast(ts-? as integer)%86400)/3600, count(*) {base} group by 1, 2"),
        &args(&[json!(off), json!(off)], &ab),
    );
    Groups { series, by_bot, models, roles, hours }
}

fn tasks(a: f64, b: f64, bot: Option<i64>) -> Vec<Value> {
    let (ws, ba) = bot_sql(bot, "bot_id");
    q(
        &format!(
            "select id, ts, content, meta, chat_id, bot_id from messages where {} and ts>=? and ts<?{ws} order by ts desc",
            tasklog::DISPATCH
        ),
        &args(&[json!(a), json!(b)], &ba),
    )
    .into_iter()
    .map(|r| {
        let m = r.j(3);
        let content = r.s(2);
        let hit = ROUTINE.captures(&content).map(|c| c[1].to_string());
        let goal = ROUTINE.replace(&content, "").into_owned();
        let goal = goal.strip_prefix("[任务已派发]").unwrap_or(&goal).trim().to_string();
        json!({"id": r[0], "ts": r[1], "goal": goal, "status": m.get("status").cloned().unwrap_or(json!("done")),
               "elapsed": m.get("elapsed").cloned().unwrap_or(Value::Null), "steps": m.get("steps").cloned().unwrap_or(Value::Null),
               "mode": m.get("mode").cloned().unwrap_or(json!("single")), "quiet": py::truthy(&m["quiet"]), "chat_id": r[4], "bot_id": r[5],
               "routine": hit})
    })
    .collect()
}

fn task_kpi(ts: &[Value]) -> Value {
    let keys = ["done", "failed", "cancelled", "running", "interrupted"];
    let mut n: BTreeMap<&str, i64> = keys.iter().map(|k| (*k, 0)).collect();
    for t in ts {
        let s = t["status"].as_str().unwrap_or("");
        *n.get_mut(if keys.contains(&s) { s } else { "done" }).unwrap() += 1;
    }
    let ended = n["done"] + n["failed"] + n["interrupted"];
    let took: Vec<f64> =
        ts.iter().filter(|t| t["status"] == "done" && py::truthy(&t["elapsed"])).filter_map(|t| t["elapsed"].as_f64()).collect();
    let routine = |t: &&Value| !t["routine"].is_null();
    let mut o = json!({"tasks": ts.len()});
    for k in keys {
        o[k] = json!(n[k]);
    }
    o["success"] = if ended > 0 { json!(py::round_n(n["done"] as f64 / ended as f64, 3)) } else { Value::Null };
    o["avg_s"] = if took.is_empty() { Value::Null } else { json!(py::round0(took.iter().sum::<f64>() / took.len() as f64)) };
    o["routine_runs"] = json!(ts.iter().filter(routine).count());
    o["routine_failed"] = json!(ts.iter().filter(routine).filter(|t| t["status"] == "failed" || t["status"] == "interrupted").count());
    o["routine_quiet"] = json!(ts.iter().filter(routine).filter(|t| t["quiet"] == true).count());
    o
}

fn costs(ids: &[i64]) -> std::collections::HashMap<i64, (f64, i64)> {
    if ids.is_empty() {
        return Default::default();
    }
    let marks = vec!["?"; ids.len()].join(",");
    db::rows(
        &format!("select task_id, sum(cost), sum(cache_hit+cache_miss+output) from llm_calls where task_id in ({marks}) group by 1"),
        rusqlite::params_from_iter(ids.iter()),
    )
    .into_iter()
    .map(|r| (r.i(0), (r.f(1), r.i(2))))
    .collect()
}

fn tools(a: f64, b: f64, bot: Option<i64>) -> Vec<Value> {
    let (ws, ba) = bot_sql(bot, "m.bot_id");
    q(
        &format!(
            "select coalesce(nullif(s.name,''), s.title), count(*),
        sum(case when s.status='error' then 1 else 0 end), sum(case when s.status='rejected' then 1 else 0 end),
        avg(case when s.status='done' then s.dur end), max(s.ts)
        from task_steps s join messages m on m.id=s.task_id where s.kind='tool' and s.ts>=? and s.ts<?{ws}
        group by 1 order by 2 desc"
        ),
        &args(&[json!(a), json!(b)], &ba),
    )
    .into_iter()
    .map(|r| {
        json!({"name": r[0], "calls": r[1], "errors": r[2], "rejected": r[3],
               "avg_s": r[4].as_f64().map(|d| json!(py::round_n(d, 1))).unwrap_or(Value::Null), "last": r[5]})
    })
    .collect()
}

fn problems(a: f64, b: f64, bot: Option<i64>, ts: &[Value]) -> Vec<Value> {
    let mut items: Vec<Value> = vec![];
    let bad: Vec<&Value> = ts.iter().filter(|t| t["status"] == "failed" || t["status"] == "interrupted").collect();
    if !bad.is_empty() {
        let marks = vec!["?"; bad.len()].join(",");
        let ids: Vec<Value> = bad.iter().map(|t| t["id"].clone()).collect();
        let why: std::collections::HashMap<i64, String> = q(
            &format!(
                "select json_extract(meta, '$.task_id'), content from messages where role='task'
            and json_extract(meta, '$.kind')='result' and json_extract(meta, '$.task_id') in ({marks})"
            ),
            &ids,
        )
        .into_iter()
        .map(|r| (r.i(0), r.s(1)))
        .collect();
        for t in bad {
            let raw = why.get(&t["id"].as_i64().unwrap_or(0)).cloned().unwrap_or_default();
            let detail = py::head(&RESULT.replace(&raw, ""), 300);
            items.push(json!({"kind": "task", "status": t["status"], "ts": t["ts"], "bot_id": t["bot_id"], "task_id": t["id"],
                              "chat_id": t["chat_id"], "title": py::head(t["goal"].as_str().unwrap_or(""), 120), "detail": detail, "routine": t["routine"]}));
        }
    }
    let (ws, ba) = bot_sql(bot, "bot_id");
    for r in q(
        &format!(
            "select ts, bot_id, role, provider, model, err, task_id, chat_id from llm_calls where err is not null and ts>=? and ts<?{ws} order by ts desc limit 500"
        ),
        &args(&[json!(a), json!(b)], &ba),
    ) {
        let provider = r.s(3);
        items.push(json!({"kind": "model", "ts": r[0], "bot_id": r[1], "task_id": r[6], "chat_id": r[7],
                          "title": format!("{}{}", if provider.is_empty() { String::new() } else { format!("{provider}/") }, r.s(4)), "detail": r[5], "role": r[2]}));
    }
    let (ws, ba) = bot_sql(bot, "m.bot_id");
    for r in q(
        &format!(
            "select s.ts, coalesce(nullif(s.name,''), s.title), s.body, s.task_id, m.bot_id, m.chat_id
            from task_steps s join messages m on m.id=s.task_id where s.kind='tool' and s.status='error'
            and s.ts>=? and s.ts<?{ws} order by s.ts desc limit 500"
        ),
        &args(&[json!(a), json!(b)], &ba),
    ) {
        items.push(json!({"kind": "tool", "ts": r[0], "bot_id": r[4], "task_id": r[3], "chat_id": r[5], "title": r[1], "detail": py::head(&r.s(2), 300)}));
    }
    items
        .sort_by(|x, y| y["ts"].as_f64().unwrap_or(0.0).partial_cmp(&x["ts"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    let mut keys: Vec<String> = vec![];
    let mut merged: Vec<Value> = vec![];
    for it in items {
        let key = if it["kind"] != "task" {
            py::dumps_u(&json!([it["kind"], it["bot_id"], it["title"], py::head(&py::str_of(&it["detail"]), 60)]))
        } else {
            py::dumps_u(&json!(["task", it["task_id"]]))
        };
        match keys.iter().position(|k| *k == key) {
            Some(i) => {
                let c = merged[i]["count"].as_i64().unwrap_or(1);
                merged[i]["count"] = json!(c + 1);
            }
            None => {
                keys.push(key);
                let mut it = it;
                it["count"] = json!(1);
                merged.push(it);
            }
        }
    }
    merged.truncate(TOP);
    merged
}

fn routines_of(bot: Option<i64>, ts: &[Value], costs: &std::collections::HashMap<i64, (f64, i64)>) -> Vec<Value> {
    if bot == Some(UNKNOWN) {
        return vec![];
    }
    crate::routines::list_for(bot)
        .into_iter()
        .map(|r| {
            let mine: Vec<&Value> = ts.iter().filter(|t| t["routine"] == r["name"] && t["bot_id"] == r["bot_id"]).collect();
            let mut o = json!({"id": r["id"], "bot_id": r["bot_id"], "name": r["name"], "enabled": r["enabled"], "trigger": r["trigger"],
                "describe": r["describe"], "category": r["category"], "next_run": r["next_run"], "running": r["running"], "last_run": r["last_run"],
                "runs": mine.len()});
            for k in ["done", "failed", "cancelled", "interrupted"] {
                o[k] = json!(mine.iter().filter(|t| t["status"] == k).count());
            }
            o["quiet"] = json!(mine.iter().filter(|t| t["quiet"] == true).count());
            o["cost"] = json!(py::round_n(mine.iter().map(|t| costs.get(&t["id"].as_i64().unwrap_or(0)).map(|c| c.0).unwrap_or(0.0)).sum(), 4));
            o
        })
        .collect()
}

fn merge(a: &mut Value, b: Value) {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        for (k, v) in b {
            a.insert(k, v);
        }
    }
}

pub fn stats(rng: &str, tz: i64, bot: Option<i64>, now: Option<f64>) -> Value {
    let w = window(rng, tz, now);
    let rng = if range_days(rng).is_some() { rng } else { "7d" };
    let (a, b, size) = (w.start, w.end, w.size);
    let n = floor_div(b - a + size - 1.0, size) as i64;
    let n = n.max(0) as usize;
    let g = usage_groups(&w, tz, bot);
    let ts = tasks(a, b, bot);
    let cs = costs(&ts.iter().filter_map(|t| t["id"].as_i64()).collect::<Vec<_>>());

    // 趋势
    let mut series: Vec<Value> = (0..n)
        .map(|i| json!({"t": a + i as f64 * size, "cost": 0.0, "tokens": 0, "calls": 0, "errors": 0, "tasks": 0, "bots": {}}))
        .collect();
    fn cell(series: &mut [Value], i: usize, bot_id: i64) -> &mut Value {
        let bots = series[i]["bots"].as_object_mut().unwrap();
        bots.entry(bot_id.to_string()).or_insert(json!({"cost": 0.0, "tokens": 0, "calls": 0, "tasks": 0}))
    }
    fn addf(v: &mut Value, k: &str, x: f64) {
        v[k] = json!(v[k].as_f64().unwrap_or(0.0) + x);
    }
    fn addi(v: &mut Value, k: &str, x: i64) {
        v[k] = json!(v[k].as_i64().unwrap_or(0) + x);
    }
    for r in &g.series {
        let i = r.i(0);
        if i >= 0 && (i as usize) < n {
            let i = i as usize;
            let (cost, tokens, calls, errors) = (r.f(2), r.i(3), r.i(7), r.i(8));
            let s = &mut series[i];
            addf(s, "cost", cost);
            addi(s, "tokens", tokens);
            addi(s, "calls", calls);
            addi(s, "errors", errors);
            let c = cell(&mut series, i, r.i(1));
            addf(c, "cost", cost);
            addi(c, "tokens", tokens);
            addi(c, "calls", calls);
        }
    }
    for t in &ts {
        let i = floor_div(t["ts"].as_f64().unwrap_or(0.0) - a, size);
        if i >= 0.0 && (i as usize) < n {
            let i = i as usize;
            addi(&mut series[i], "tasks", 1);
            addi(cell(&mut series, i, t["bot_id"].as_i64().unwrap_or(UNKNOWN)), "tasks", 1);
        }
    }
    for s in series.iter_mut() {
        s["cost"] = json!(py::round_n(s["cost"].as_f64().unwrap_or(0.0), 4));
        for c in s["bots"].as_object_mut().unwrap().values_mut() {
            c["cost"] = json!(py::round_n(c["cost"].as_f64().unwrap_or(0.0), 4));
        }
    }

    // 按 Bot
    let mut bots: indexmap::IndexMap<i64, Value> = indexmap::IndexMap::new();
    fn bot_row(bots: &mut indexmap::IndexMap<i64, Value>, i: i64) -> &mut Value {
        bots.entry(i).or_insert(json!({"bot_id": i, "cost": 0.0, "tokens": 0, "calls": 0, "errors": 0, "ttft": null,
                                       "tasks": 0, "done": 0, "failed": 0, "avg_s": null, "routine_runs": 0}))
    }
    for r in &g.by_bot {
        let row = bot_row(&mut bots, r.i(0));
        merge(
            row,
            json!({"cost": py::round_n(r.f(1), 4), "tokens": r[2], "calls": or0(&r[6]), "errors": or0(&r[7]), "ttft": round_opt(&r[8])}),
        );
    }
    let mut ids: Vec<Option<i64>> = vec![];
    for t in &ts {
        let b = t["bot_id"].as_i64();
        if !ids.contains(&b) {
            ids.push(b);
        }
    }
    ids.sort_by_key(|b| (b.is_none(), b.unwrap_or(0)));
    for bid in ids {
        let mine: Vec<Value> = ts.iter().filter(|t| t["bot_id"].as_i64() == bid).cloned().collect();
        let k = task_kpi(&mine);
        let row = bot_row(&mut bots, bid.unwrap_or(UNKNOWN));
        merge(
            row,
            json!({"tasks": k["tasks"], "done": k["done"], "failed": k["failed"].as_i64().unwrap_or(0) + k["interrupted"].as_i64().unwrap_or(0),
                          "avg_s": k["avg_s"], "success": k["success"], "routine_runs": k["routine_runs"]}),
        );
    }
    let mut bots: Vec<Value> = bots.into_values().collect();
    bots.sort_by(|x, y| {
        let k = |v: &Value| (-(v["cost"].as_f64().unwrap_or(0.0)), -(v["tasks"].as_i64().unwrap_or(0)));
        k(x).partial_cmp(&k(y)).unwrap_or(std::cmp::Ordering::Equal)
    });

    let (bw, bargs) = bot_sql(bot, "bot_id");
    let mut models: Vec<Value> = g
        .models
        .iter()
        .map(|r| {
            let (hit, miss) = (r.i(4), r.i(5));
            json!({"provider": r[0], "model": r[1], "cost": py::round_n(r.f(2), 4), "tokens": r[3], "output": r[6],
                   "cache_rate": rate(hit, miss), "calls": or0(&r[7]), "errors": or0(&r[8]), "ttft": round_opt(&r[9]), "last": r[10],
                   "speed": speed(&format!("ts>=? and ts<? and coalesce(provider,'')=? and model=?{bw}"), &args(&[json!(a), json!(b), r[0].clone(), r[1].clone()], &bargs))})
        })
        .collect();
    models.sort_by(|x, y| {
        let k = |v: &Value| (-(v["cost"].as_f64().unwrap_or(0.0)), -(v["calls"].as_i64().unwrap_or(0) as f64));
        k(x).partial_cmp(&k(y)).unwrap_or(std::cmp::Ordering::Equal)
    });

    let tl = tools(a, b, bot);
    let mut kpi = usage_kpi(a, b, bot);
    merge(&mut kpi, task_kpi(&ts));
    kpi["tool_calls"] = json!(tl.iter().map(|x| x["calls"].as_i64().unwrap_or(0)).sum::<i64>());
    kpi["tool_errors"] = json!(tl.iter().map(|x| x["errors"].as_i64().unwrap_or(0)).sum::<i64>());
    let prev = w.prev.map(|(pa, pb)| {
        let mut p = usage_kpi(pa, pb, bot);
        merge(&mut p, task_kpi(&tasks(pa, pb, bot)));
        p
    });
    let recent: Vec<Value> = ts
        .iter()
        .take(TOP)
        .map(|t| {
            let c = cs.get(&t["id"].as_i64().unwrap_or(0)).copied().unwrap_or((0.0, 0));
            let mut t = t.clone();
            t["cost"] = json!(py::round_n(c.0, 4));
            t["tokens"] = json!(c.1);
            t
        })
        .collect();
    let mut heat = vec![vec![0i64; 24]; 7];
    for r in &g.hours {
        let (d, h) = (r.i(0), r.i(1));
        if (0..7).contains(&d) && (0..24).contains(&h) {
            heat[d as usize][h as usize] = r.i(2);
        }
    }
    let roles: Vec<Value> = g
        .roles
        .iter()
        .map(|r| json!({"role": r[0], "cost": py::round_n(r.f(1), 4), "tokens": r[2], "calls": or0(&r[6]), "errors": or0(&r[7])}))
        .collect();
    json!({
        "currency": config::get("pricing.currency"), "range": rng, "start": a, "end": b, "size": size,
        "kpi": kpi, "prev": prev, "series": series, "bots": bots, "models": models, "roles": roles,
        "tools": tl, "routines": routines_of(bot, &ts, &cs), "tasks": recent, "problems": problems(a, b, bot, &ts), "hours": heat,
    })
}

pub const CALL_COLS: [&str; 15] =
    ["id", "ts", "bot_id", "chat_id", "task_id", "role", "provider", "model", "input", "cache_hit", "output", "cost", "ms", "ttft", "err"];

pub fn calls(rng: &str, tz: i64, bot: Option<i64>, before: Option<i64>, limit: i64, failed: bool, now: Option<f64>) -> Value {
    let w = window(rng, tz, now);
    let (ws, ba) = bot_sql(bot, "bot_id");
    let wh = format!("ts>=? and ts<?{ws}{}", if failed { " and err is not null" } else { "" });
    let sql = format!(
        "select id, ts, bot_id, chat_id, task_id, role, provider, model, cache_hit+cache_miss, cache_hit, output,
        cost, ms, ttft, err from llm_calls where {wh}"
    );
    let base = args(&[json!(w.start), json!(w.end)], &ba);
    let rows = q(
        &format!("{sql} and id<? order by id desc limit ?"),
        &args(&base, &[json!(before.filter(|b| *b != 0).unwrap_or(crate::memory::BIG)), json!(limit + 1)]),
    );
    let total = q(&format!("select count(*) from llm_calls where {wh}"), &base).into_iter().next().map(|r| r.i(0)).unwrap_or(0);
    let items: Vec<Value> = rows
        .iter()
        .take(limit.max(0) as usize)
        .map(|r| Value::Object(CALL_COLS.iter().zip(r.iter()).map(|(k, v)| (k.to_string(), v.clone())).collect::<Map<_, _>>()))
        .collect();
    json!({"items": items, "more": rows.len() as i64 > limit, "total": total})
}

fn csv_field(v: &Value) -> String {
    let s = match v {
        Value::Null => String::new(),
        v => py::str_of(v),
    };
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

pub fn calls_csv(rng: &str, tz: i64, bot: Option<i64>, names: &std::collections::HashMap<i64, String>) -> String {
    let w = window(rng, tz, None);
    let (ws, ba) = bot_sql(bot, "bot_id");
    let rows = q(
        &format!(
            "select id, ts, bot_id, chat_id, task_id, role, provider, model, cache_hit+cache_miss, cache_hit,
        output, cost, ms, ttft, err from llm_calls where ts>=? and ts<?{ws} order by id"
        ),
        &args(&[json!(w.start), json!(w.end)], &ba),
    );
    let mut out = String::from("\u{feff}");
    let head: Vec<String> = ["time", "bot"].iter().map(|s| s.to_string()).chain(CALL_COLS[2..].iter().map(|s| s.to_string())).collect();
    out.push_str(&head.join(","));
    out.push_str("\r\n");
    for r in rows {
        let t = r.f(1) - (tz * 60) as f64;
        let local =
            chrono::DateTime::from_timestamp(t.floor() as i64, 0).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
        let name = r.oi(2).and_then(|b| names.get(&b).cloned()).unwrap_or_default();
        let mut cols = vec![csv_field(&json!(local)), csv_field(&json!(name))];
        cols.extend(r[2..].iter().map(csv_field));
        out.push_str(&cols.join(","));
        out.push_str("\r\n");
    }
    out
}

// ---------- 接口 ----------
fn obs_args(q: &std::collections::HashMap<String, String>) -> (String, i64, Option<i64>) {
    let rng = qs(q, "range").unwrap_or("7d");
    let rng = if range_days(rng).is_some() { rng.to_string() } else { "7d".into() };
    (rng, qi(q, "tz").unwrap_or(0).clamp(-840, 840), qi(q, "bot"))
}

fn qbool(q: &std::collections::HashMap<String, String>, k: &str) -> bool {
    q.get(k).is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on" | "y" | "t"))
}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/usage", get(usage))
        .route("/api/status", get(status))
        .route("/api/observe", get(observe_stats))
        .route("/api/observe/calls", get(observe_calls))
        .route("/api/observe/calls.csv", get(observe_calls_csv))
}

async fn usage() -> Res {
    ok(crate::memory::usage_stats())
}

async fn status() -> Res {
    ok(
        json!({"mcp": crate::mcp_hub::status_strings(), "mcp_tools": crate::mcp_hub::all_tool_names(), "mcp_confirm": crate::mcp_hub::confirm_list(),
              "routines": crate::routines::list_for(None).iter().map(|r| json!({"id": r["id"], "bot_id": r["bot_id"], "name": r["name"],
                   "enabled": r["enabled"], "trigger": crate::routines::describe(&r["trigger"], None)})).collect::<Vec<_>>()}),
    )
}

async fn observe_stats(q: Q) -> Res {
    let (rng, tz, bot) = obs_args(&q);
    ok(stats(&rng, tz, bot, None))
}

async fn observe_calls(q: Q) -> Res {
    let (rng, tz, bot) = obs_args(&q);
    let limit = qi(&q, "limit").unwrap_or(50).clamp(1, 200);
    ok(calls(&rng, tz, bot, qi(&q, "before"), limit, qbool(&q, "failed"), None))
}

async fn observe_calls_csv(q: Q) -> Response {
    let (rng, tz, bot) = obs_args(&q);
    let names = bots::list_bots().into_iter().filter_map(|b| Some((b["id"].as_i64()?, py::str_of(&b["name"])))).collect();
    let body = calls_csv(&rng, tz, bot, &names);
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"osa-calls-{}.csv\"", py::strftime("%Y%m%d"))),
        ],
        body,
    )
        .into_response()
}
