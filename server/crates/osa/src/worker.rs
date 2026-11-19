//! Worker：隔离的 ReAct 循环，带工具，单独执行一个任务。模型流式输出，每一步都记进任务过程（tasklog）。
//! 对应 osa/worker.py。

use crate::computer::tools as ctools;
use crate::registry::{self, Call, Out, WORKER};
use crate::task::Task;
use crate::tasklog::{Log, StepRef};
use crate::{approvals, bots, config, dsml, i18n, memories, memory, providers, py, router, watchdog};
use futures::future::join_all;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

pub const WORKER_PROMPT: &str = "你是执行任务的 Worker。用工具查证事实，给出结论时附上来源链接。
需要读写用户的外部服务（应用）时，使用名字带“应用名__”前缀的工具；任务里写了“@应用名”就必须用那个应用。
任务里写了按技能 /名字 做、却没附上做法或要用技能带的文件时，先用 use_skill 取。
写操作会先请用户确认；被拒绝时不要换个工具绕过去，按用户的意见调整或说明无法完成。
算数、日期、单位换算、统计和处理数据，有 run_code 时用它在沙箱里跑代码算，不要心算。
完成后直接输出最终答案（{lang}，简洁、结构清晰）。今天是 {date}。";

fn answer_lang() -> &'static str {
    if i18n::lang() == "en" { "默认用英文，任务是用别的语言写的就用那种语言" } else { "中文" }
}

pub const APPLET_GUIDE: &str = "做小应用（可交互的界面）时 format=html 并填 app，页面里直接用 window.osa（服务器会自动注入，不用引脚本）：
  osa.call(工具名, 参数对象) → 文字结果（只能用 app.tools 里列的工具；要确认的会弹卡片）；
  osa.send(文字) 替用户给你发一条消息（要 app.chat=true），你会像收到用户消息一样接着处理；
  osa.store.get(键) / set(键, 值) / all() / remove(键) 读写这个小应用的数据，你用 page_data 工具也能读写同一份数据，
  osa.on('data', fn) 在数据被改时收到通知。页面高度会自动适应内容，不要写死 100vh；不要用 fetch，外网和接口都连不上。
用户在对话里会直接看到小应用并能操作，所以回复里附上链接就行，不用复述界面内容。";

fn page_hint() -> String {
    format!("任务要求做成页面或报告时，用 save_page 保存，最终答案里附上返回的链接。\n{APPLET_GUIDE}")
}

pub const FILE_HINT: &str =
    "要把电脑里的文件（截图、画好的图、做好的表格和文档）交给用户时用 send_file，它们会显示在任务结果下面；不要自己拼下载链接。";

pub async fn run_worker(task: Arc<Task>, goal: String, label: String) -> anyhow::Result<String> {
    let claim = format!("{}:{label}", task.id);
    let who = memory::Who { bot_id: Some(task.bot_id), chat_id: Some(task.chat_id), task_id: Some(task.id) };
    let t2 = task.clone();
    let fut = async move {
        let r = run_inner(t2.clone(), &goal, &label).await;
        ctools::release().await;
        r
    };
    ctools::CLAIM.scope(claim, memory::WHO.scope(who, fut)).await
}

/// 流式调用一次模型，思考和正文边收边写进任务过程。返回这一轮的 assistant 消息
pub async fn reply(
    task: &Arc<Task>,
    log: &Log,
    role: &str,
    client: &providers::Client,
    model: &str,
    msgs: &[Value],
    tools: Option<&[Value]>,
) -> anyhow::Result<Value> {
    let t0 = Instant::now();
    let mut body = json!({"model": model, "messages": msgs, "stream": true, "stream_options": {"include_usage": true}});
    if let Some(ts) = tools.filter(|t| !t.is_empty()) {
        body["tools"] = json!(ts);
    }
    for (k, v) in config::call_kwargs(role) {
        body[k] = v;
    }
    let rec = || memory::UsageRec {
        task_id: Some(task.id),
        bot_id: Some(task.bot_id),
        chat_id: Some(task.chat_id),
        ms: Some(t0.elapsed().as_secs_f64() * 1000.0),
        ..Default::default()
    };
    let mut stream = match client.stream(body).await {
        Ok(s) => s,
        Err(e) => {
            memory::record_error(role, model, &providers::explain(&e, &e.origin), rec());
            return Err(e.into());
        }
    };
    let (mut content, mut reasoning) = (String::new(), String::new());
    let mut calls: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
    let mut usage: Option<Value> = None;
    let mut first: Option<Instant> = None;
    let mut think: Option<StepRef> = None;
    let mut text: Option<StepRef> = None;
    let mut sent = 0usize;
    let say = |piece: &str, think: &Option<StepRef>, text: &mut Option<StepRef>| {
        if let Some(th) = think {
            log.finish(th, "done", None, None);
        }
        if text.as_ref().is_none_or(|t| t.lock().status != "running") {
            *text = Some(log.start("text", "", "", "running"));
        }
        log.append(text.as_ref().unwrap(), piece);
    };
    loop {
        let chunk = match stream.next().await {
            None => break,
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                memory::record_error(role, model, &providers::explain(&e, &e.origin), rec());
                return Err(e.into());
            }
        };
        task.touch();
        if chunk.get("usage").is_some_and(|u| !u.is_null()) {
            usage = Some(chunk["usage"].clone());
        }
        let Some(d) = chunk["choices"].as_array().and_then(|c| c.first()).map(|c| c["delta"].clone()) else { continue };
        let rc = d["reasoning_content"].as_str().unwrap_or("");
        let dc = d["content"].as_str().unwrap_or("");
        let tcs = d["tool_calls"].as_array().cloned().unwrap_or_default();
        if first.is_none() && (!dc.is_empty() || !rc.is_empty() || !tcs.is_empty()) {
            first = Some(Instant::now());
        }
        if !rc.is_empty() {
            reasoning.push_str(rc);
            if think.as_ref().is_none_or(|t| t.lock().status != "running") {
                think = Some(log.start("think", "", "", "running"));
            }
            log.append(think.as_ref().unwrap(), rc);
        }
        if !dc.is_empty() {
            content.push_str(dc);
            let n = dsml::visible(&content);
            if n > sent {
                let piece = content[sent..n].to_string();
                say(&piece, &think, &mut text);
                sent = n;
            }
        }
        for tc in tcs {
            let idx = tc["index"].as_i64().unwrap_or(0);
            let c = calls.entry(idx).or_default();
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                c.0 = id.to_string();
            }
            if tc["function"].is_object() {
                c.1.push_str(tc["function"]["name"].as_str().unwrap_or(""));
                c.2.push_str(tc["function"]["arguments"].as_str().unwrap_or(""));
            }
        }
    }
    memory::record_usage(
        role,
        model,
        usage.as_ref(),
        memory::UsageRec {
            task_id: Some(task.id),
            bot_id: Some(task.bot_id),
            chat_id: Some(task.chat_id),
            ms: Some(t0.elapsed().as_secs_f64() * 1000.0),
            ttft: first.map(|f| (f - t0).as_secs_f64() * 1000.0),
            ..Default::default()
        },
    );
    let (content, leaked) = dsml::split(&content);
    if content.len() > sent {
        let piece = content[sent..].to_string();
        say(&piece, &think, &mut text);
    }
    let has_tools = tools.is_some_and(|t| !t.is_empty());
    if calls.is_empty() && has_tools {
        for (k, l) in leaked.into_iter().enumerate() {
            calls.insert(k as i64, (l.id, l.name, l.args));
        }
    }
    if let Some(th) = &think {
        log.finish(th, "done", None, None);
    }
    if let Some(tx) = &text {
        log.finish(tx, "done", None, Some(if calls.is_empty() { "answer" } else { "say" }));
    }
    let mut msg = json!({"role": "assistant"});
    if !content.is_empty() {
        msg["content"] = json!(content);
    }
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        msg["tool_calls"] = json!(
            calls
                .iter()
                .map(|(k, (id, name, args))| json!({
            "id": if id.is_empty() { format!("call_{k}") } else { id.clone() }, "type": "function",
            "function": {"name": name, "arguments": if args.is_empty() { "{}" } else { args }}}))
                .collect::<Vec<_>>()
        );
    }
    Ok(msg)
}

fn progress_text(tool: &registry::Tool, name: &str, args: &Value) -> String {
    match &tool.progress {
        Some(p) => p(args),
        None => {
            let q = if py::truthy(&args["query"]) { &args["query"] } else { &args["url"] };
            let q = if q.is_null() { String::new() } else { py::str_of(q) };
            format!("{name} {}", py::head(&q, 60))
        }
    }
}

/// 执行一个工具调用，返回 (给模型的结果, 这一步的状态)
async fn call_tool(
    task: &Arc<Task>,
    log: &Log,
    step: &StepRef,
    name: &str,
    raw: &str,
    env: &Value,
    shots: &mut Vec<Vec<u8>>,
) -> (String, String) {
    let args: Value = match serde_json::from_str(if raw.is_empty() { "{}" } else { raw }) {
        Ok(v) => v,
        Err(e) => return (format!("工具出错：{e}"), "error".into()),
    };
    let progress = |text: String| {
        task.st.lock().last_action = text.clone();
        log.update(step, Some(&text), None);
        router::progress(task);
    };
    if ctools::is_name(name) {
        let (out, shot) = ctools::call(task, name, &args, &progress, None).await;
        if let Some(s) = shot {
            shots.push(s);
        }
        return (out, "done".into());
    }
    let Some(tool) = registry::available(name, WORKER, env) else {
        return ("未知工具".into(), "error".into());
    };
    progress(progress_text(&tool, name, &args));
    let mut args = args;
    if tool.gated && approvals::decide(name, None, &args).0 {
        log.update(step, None, Some("waiting"));
        let (ok, note, after) = approvals::request(task, name, &args, approvals::Req::default()).await;
        if !ok {
            return (format!("用户拒绝了这个操作，没有执行。{note}").trim().to_string(), "rejected".into());
        }
        args = after;
        log.update(step, None, Some("running"));
    }
    let bot = bots::get(task.bot_id).unwrap_or(json!({"id": task.bot_id}));
    let call = Call {
        scope: WORKER,
        bot,
        chat_id: task.chat_id,
        private_chat_id: if task.approval_chat_id != 0 { task.approval_chat_id } else { task.chat_id },
        args,
        task: Some(task.clone()),
        post: None,
        files: task.files.clone(),
        apps: task.apps.clone(),
        env: env.clone(),
    };
    let run = tool.run.clone();
    // 工具 panic 了也当成工具出错，别把任务弄挂
    use futures::FutureExt;
    match std::panic::AssertUnwindSafe(watchdog::TASK.scope(task.clone(), run(call))).catch_unwind().await {
        Ok(Out::Text(s)) => (s, "done".into()),
        Ok(Out::Shot(s, png)) => {
            if let Some(p) = png {
                shots.push(p);
            }
            (s, "done".into())
        }
        Err(_) => ("工具出错：内部错误".into(), "error".into()),
    }
}

/// 这个调用能不能和相邻的只读调用一起跑：能就返回 (组, 同组同时最多几个)
fn lane(call: &Value, env: &Value) -> Option<(String, usize)> {
    let name = call["function"]["name"].as_str().unwrap_or("");
    if ctools::is_name(name) {
        return None;
    }
    let tool = registry::available(name, WORKER, env)?;
    if tool.parallel < 1 {
        return None;
    }
    let raw = call["function"]["arguments"].as_str().unwrap_or("");
    let args: Value = serde_json::from_str(if raw.is_empty() { "{}" } else { raw }).ok()?;
    if !args.is_object() || (tool.gated && approvals::decide(name, None, &args).0) {
        return None;
    }
    Some((if tool.group.is_empty() { tool.name.clone() } else { tool.group.clone() }, tool.parallel))
}

async fn run_calls(task: &Arc<Task>, log: &Log, calls: &[Value], env: &Value) -> Vec<(String, Vec<Vec<u8>>)> {
    let mut results = vec![];
    let mut i = 0;
    while i < calls.len() {
        let mut lanes = vec![lane(&calls[i], env)];
        while lanes[0].is_some() && i + lanes.len() < calls.len() {
            match lane(&calls[i + lanes.len()], env) {
                Some(l) => lanes.push(Some(l)),
                None => break,
            }
        }
        let group = &calls[i..i + lanes.len()];
        let mut steps = vec![];
        for call in group {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let raw = call["function"]["arguments"].as_str().unwrap_or("").to_string();
            {
                let mut s = task.st.lock();
                s.recent_calls.push(format!("{name}:{raw}"));
                let n = s.recent_calls.len();
                if n > 6 {
                    s.recent_calls.drain(..n - 6);
                }
            }
            steps.push(log.start("tool", &name, &py::head(&raw, 2000), "running"));
        }
        let sems: HashMap<String, Arc<Semaphore>> = if group.len() > 1 {
            lanes.iter().flatten().map(|(g, n)| (g.clone(), Arc::new(Semaphore::new(*n)))).collect()
        } else {
            HashMap::new()
        };
        let futs = group.iter().zip(steps.iter()).zip(lanes.iter()).map(|((call, step), ln)| {
            let sem = ln.as_ref().and_then(|(g, _)| sems.get(g).cloned());
            let task = task.clone();
            let log = log.clone();
            let step = step.clone();
            let env = env.clone();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let raw = call["function"]["arguments"].as_str().unwrap_or("").to_string();
            async move {
                let mut shots = vec![];
                let _permit = match &sem {
                    Some(s) => Some(s.acquire().await),
                    None => None,
                };
                let (out, status) = call_tool(&task, &log, &step, &name, &raw, &env, &mut shots).await;
                drop(_permit);
                log.finish(&step, &status, Some(&out), None);
                task.touch();
                (out, shots)
            }
        });
        results.extend(join_all(futs).await);
        i += group.len();
    }
    results
}

async fn run_inner(task: Arc<Task>, goal: &str, label: &str) -> anyhow::Result<String> {
    let (client, model) = providers::client_for("worker")?;
    let use_pc = config::get_bool("computer.enabled", true);
    let pages = label.is_empty();
    let mut system = WORKER_PROMPT.replace("{date}", &py::strftime("%Y-%m-%d")).replace("{lang}", answer_lang());
    if pages {
        system += &format!("\n{}", page_hint());
    }
    if pages && use_pc {
        system += &format!("\n{FILE_HINT}");
    }
    if use_pc {
        system += &format!("\n\n{}", ctools::system_hint());
    }
    let known = memories::worker_block(task.bot_id);
    if !known.is_empty() {
        system += &format!("\n\n关于用户（长期记忆，做事时遵守其中的偏好）：\n{known}");
    }
    let mut msgs = vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": goal})];
    let env = json!({"scope": WORKER, "pages": pages, "use_pc": use_pc});
    let mut schemas = registry::schemas(WORKER, &env);
    if use_pc {
        schemas.extend(ctools::schemas());
    }
    let log = Log::new(&task, label);
    let max = config::get_i64("limits.worker_max_steps", 30);
    for _ in 0..max {
        ctools::prune_images(&mut msgs, 2);
        let m = reply(&task, &log, "worker", &client, &model, &msgs, Some(&schemas)).await?;
        {
            let mut s = task.st.lock();
            s.steps += 1;
            s.last_progress = py::now();
        }
        let Some(calls) = m["tool_calls"].as_array().cloned().filter(|c| !c.is_empty()) else {
            return Ok(m["content"].as_str().unwrap_or("").to_string());
        };
        msgs.push(m);
        let mut shots = vec![];
        for (call, (out, got)) in calls.iter().zip(run_calls(&task, &log, &calls, &env).await) {
            msgs.push(json!({"role": "tool", "tool_call_id": call["id"], "content": py::head(&out, 8000)}));
            shots.extend(got);
        }
        if let Some(s) = shots.last() {
            msgs.push(ctools::image_message(s, None, true));
        }
    }
    log.note(&t!("步数用完了，根据已有信息写结果"), "done");
    msgs.push(json!({"role": "user", "content": "步数用完了，请根据已有信息给出最终答案。"}));
    let m = reply(&task, &log, "worker", &client, &model, &msgs, None).await?;
    Ok(m["content"].as_str().unwrap_or("").to_string())
}

/// Heavy 模式：N 个 Worker 独立并行解题，Reviewer 互审后汇总
pub async fn run_heavy(task: Arc<Task>) -> anyhow::Result<String> {
    let n = config::get_i64("heavy.workers", 3).max(1) as usize;
    let angles = ["从最直接的角度", "从反面/质疑的角度核查", "从数据和一手来源角度", "从实践/落地角度"];
    Log::new(&task, "").note(&t!("{n} 个研究员分头做，做完汇总", n = n), "done");
    let goal = format!("{}{}", task.goal, task.notes);
    let futs = (0..n).map(|i| {
        let task = task.clone();
        let g = format!("{goal}\n\n（你是第 {} 个独立研究员，请{}回答）", i + 1, angles[i % angles.len()]);
        async move { run_worker(task, g, format!("h{i}")).await }
    });
    let drafts = join_all(futs).await;
    let mut ok = vec![];
    for (i, d) in drafts.into_iter().enumerate() {
        match d {
            Ok(s) => ok.push(s),
            Err(e) => {
                Log::new(&task, &format!("h{i}")).note(&t!("出错：{error}", error = e.to_string()), "error");
            }
        }
    }
    let (client, model) = providers::client_for("reviewer")?;
    let joined = ok.iter().enumerate().map(|(i, d)| format!("=== 方案 {} ===\n{d}", i + 1)).collect::<Vec<_>>().join("\n\n");
    Log::new(&task, "").note(&t!("{n} 份草稿交给审稿人汇总", n = ok.len()), "done");
    let msgs = vec![
        json!({"role": "system", "content": format!("你是审稿人。比较几份独立答案，找出矛盾和错误，合成一份最可靠的最终答案，保留来源链接。{}", i18n::model_hint())}),
        json!({"role": "user", "content": format!("问题：{goal}\n\n{joined}")}),
    ];
    let m = reply(&task, &Log::new(&task, "review"), "reviewer", &client, &model, &msgs, None).await?;
    Ok(m["content"].as_str().unwrap_or("").to_string())
}
