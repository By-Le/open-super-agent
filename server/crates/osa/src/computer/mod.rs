//! 所有 Bot 共用一台电脑，几个任务可以同时用，各自一个浏览器窗口（见 base.rs）。对应 osa/computer/__init__.py。
//!
//! 后端由 config.yaml 的 computer.backend 决定（设置里也能换）：host（这台 Mac 的 Chrome，第 2 步 B）/ local（Docker 沙盒）/
//! instabox（云电脑）；auto 是桌面 App 里用 host，否则有 Instabox 的 API key 用 Instabox、没有用 Docker。
//! 服务器上的 Bot 还可以把某个任务派到你的 Mac 上做（mac / link，第 2 步 B）：current() 给出“这个任务用的电脑”。

pub mod api;
pub mod base;
pub mod host;
pub mod instabox;
pub mod local;
pub mod pc;
pub mod tools;

pub use base::{Computer, ComputerError, ExecResult, HOSTS};
pub use pc::{BACKENDS, available, backend_name, choices, docker_ok, docker_problem, host_allowed};

use crate::registry::Out;
use crate::task::Task;
use crate::{config, db, py, server};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

pub fn migrate() {
    db::batch("create table if not exists computers(bot_id integer primary key, backend text, ref text)");
}

pub const SHARED: i64 = 0;
static COMPUTERS: LazyLock<Mutex<HashMap<i64, Arc<Computer>>>> = LazyLock::new(Default::default);
static QUEUED: AtomicBool = AtomicBool::new(false);

/// 谁在用电脑变了：告诉前端（几件事前后脚发生时只发一次）
pub fn changed() {
    if QUEUED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(h) = tokio::runtime::Handle::try_current() else {
        QUEUED.store(false, Ordering::SeqCst);
        return;
    };
    h.spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        QUEUED.store(false, Ordering::SeqCst);
        let st = public_state(SHARED).await;
        server::broadcast(json!({"type": "computer", "computer": st}));
    });
}

fn make(bot_id: i64) -> Arc<Computer> {
    let name = backend_name();
    let backend: Box<dyn base::Backend> = match name.as_str() {
        "instabox" => {
            let fixed = config::get_str("computer.instabox_machine");
            let tpl = config::get_str("computer.instabox_template");
            let idle = config::get_i64("computer.idle_pause_s", 900);
            Box::new(instabox::Instabox::new(bot_id, if tpl.is_empty() { "agent-browser" } else { &tpl }, idle, if fixed.is_empty() { None } else { Some(fixed) }))
        }
        "host" => Box::new(host::Host::new()),
        _ => Box::new(local::Local::new()),
    };
    let c = Computer::new(bot_id, backend, Some(Arc::new(changed)));
    {
        let mut s = c.st.lock();
        s.width = config::get_i64("computer.width", 1280);
        s.height = config::get_i64("computer.height", 800);
    }
    c
}

/// 共用电脑。换了后端（设置里换了）就换一台，旧的那台暂停
pub fn get(_bot_id: i64) -> Arc<Computer> {
    let mut m = COMPUTERS.lock();
    let cur = m.get(&SHARED).cloned();
    match cur {
        Some(c) if c.kind() == backend_name() => c,
        old => {
            if let Some(o) = old {
                if let Ok(h) = tokio::runtime::Handle::try_current() {
                    h.spawn(async move {
                        let _ = o.pause().await;
                    });
                }
            }
            let c = make(SHARED);
            m.insert(SHARED, c.clone());
            c
        }
    }
}

/// 已经建好的共用电脑（没建过不建）
pub fn existing() -> Option<Arc<Computer>> {
    COMPUTERS.lock().get(&SHARED).cloned()
}

tokio::task_local! {
    /// 这个任务用的电脑：派到你的 Mac 上的任务是那台 Mac（router 在跑任务前设置），否则是共用电脑
    pub static TASK_PC: Arc<Mutex<Option<Arc<Computer>>>>;
}

/// router 跑任务时包一层：任务里能把电脑换成派到的那台 Mac（set_task_pc）
pub async fn with_task_pc<F: std::future::Future>(f: F) -> F::Output {
    TASK_PC.scope(Arc::new(Mutex::new(None)), f).await
}

pub fn set_task_pc(c: Arc<Computer>) {
    let _ = TASK_PC.try_with(|cell| *cell.lock() = Some(c));
}

pub fn current() -> Arc<Computer> {
    TASK_PC.try_with(|cell| cell.lock().clone()).ok().flatten().unwrap_or_else(|| get(SHARED))
}

pub fn users_of(c: &Computer) -> Vec<Value> {
    c.st.lock()
        .users
        .iter()
        .map(|(k, u)| json!({"key": k, "label": u.label, "task_id": u.task_id, "bot_id": u.bot_id, "window": py::truthy(&u.window)}))
        .collect()
}

/// 给前端的电脑状态
pub async fn public_state(bot_id: i64) -> Value {
    let c = get(bot_id);
    let mut o = {
        let s = c.st.lock();
        json!({"bot_id": bot_id, "backend": c.kind(), "controller": s.controller, "width": s.width, "height": s.height,
            "parallel": Computer::slots(), "users": [], "waiting": s.waiting.values().cloned().collect::<Vec<_>>(),
            "watch": s.watch, "exclusive": s.exclusive_label, "workspace": c.workspace()})
    };
    o["users"] = json!(users_of(&c));
    if HOSTS.contains(&c.kind()) {
        o["shell"] = json!(c.backend.shell_enabled());
        o["desktop"] = c.backend.desk_state(&c).await;
    }
    if let Some(st) = c.status().await.as_object() {
        for (k, v) in st {
            o[k] = v.clone();
        }
    }
    o
}

/// 闲置一段时间的电脑自动暂停
pub async fn idle_pauser() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let limit = config::get_f64("computer.idle_pause_s", 900.0);
        let all: Vec<Arc<Computer>> = COMPUTERS.lock().values().cloned().collect();
        for c in all {
            let (ctl, last) = {
                let s = c.st.lock();
                (s.controller.clone(), s.last_used)
            };
            if c.backend.auto_pause() && ctl == "bot" && py::now() - last > limit {
                let _ = c.pause().await;
            }
        }
    }
}

/// 服务停止时关掉本机电脑的 Chrome（Docker、Instabox 的电脑不管，下次接着用）
pub async fn shutdown() {
    let all: Vec<Arc<Computer>> = COMPUTERS.lock().values().cloned().collect();
    for c in all {
        if c.kind() == "host" {
            let _ = c.pause().await;
        }
    }
}

/// 这一轮说完了：对话里直接操作屏幕占着的话让出来
pub async fn desk_release(key: &str) {
    if let Some(c) = existing() {
        c.backend.desk_release(&c, key).await;
    }
}

/// look_at_screen：看一眼共用电脑现在的屏幕（Router 用）
pub async fn look_for_router() -> Out {
    let c = get(SHARED);
    let r: Result<(Vec<u8>, String), String> = async {
        if c.state().await != "running" {
            return Err(String::new());
        }
        c.look().await.map_err(|e| format!("看不了屏幕：{e}"))
    }
    .await;
    let (png, title) = match r {
        Ok(x) => x,
        Err(e) if e.is_empty() => return "电脑现在没开（已暂停或还没创建）。需要的话派任务，任务会自动开机。".into(),
        Err(e) => return e.into(),
    };
    let (exclusive, label, n, ctl) = {
        let s = c.st.lock();
        (s.exclusive.clone(), s.exclusive_label.clone(), s.users.len(), s.controller.clone())
    };
    let mut who = if !exclusive.is_empty() {
        format!("正在{label}")
    } else if n > 0 {
        format!("有 {n} 个任务在用（{}），屏幕上只显示其中一个窗口", c.busy())
    } else {
        "空闲".into()
    };
    if ctl == "human" {
        who = "用户正在接手操作".into();
    }
    Out::Shot(format!("截图见下一条消息。当前窗口：{}；电脑{who}。", if title.is_empty() { "无" } else { &title }), Some(png))
}

/// Router 自己动手做简单的电脑操作：在屏幕上看到的那个窗口里做，不占任务的位置
pub async fn direct(bot_id: i64, bot_name: &str, chat_id: i64, name: &str, args: &Value) -> Out {
    let c = get(SHARED);
    let (exclusive, label, ctl) = {
        let s = c.st.lock();
        (s.exclusive.clone(), s.exclusive_label.clone(), s.controller.clone())
    };
    if !exclusive.is_empty() {
        return format!("电脑正在{label}，先别操作。").into();
    }
    if ctl == "human" {
        return "用户正在接手电脑，先别操作。".into();
    }
    if !tools::is_desktop(name) && c.state().await != "running" {
        if let Err(e) = c.start().await {
            return format!("电脑操作失败：{e}").into();
        }
    }
    let fake = Task::fake(&format!("对话·{bot_name}"), bot_id, chat_id, "在对话里直接操作电脑", false);
    let noop = |_: String| {};
    let (text, shot) = tools::call(&fake, name, args, &noop, Some(&format!("chat-{bot_id}"))).await;
    match shot {
        Some(s) => match s.caption {
            Some(cap) => Out::Pic(text, s.png, cap),
            None => Out::Shot(text, Some(s.png)),
        },
        None => text.into(),
    }
}

// ---------- 派任务到你的 Mac（第 2 步 B） ----------
pub fn link_rows() -> Vec<Value> {
    vec![]
}

pub fn pick_mac(name: &str) -> (Option<Value>, String) {
    (None, format!("没派发：没有叫「{name}」的 Mac。用户登记过的 Mac：没有"))
}

pub async fn use_mac(task: &Arc<Task>) -> String {
    t!("「{name}」已经移除了，没有做", name = task.mac_name.clone())
}

pub async fn macs_state() -> Vec<Value> {
    vec![]
}

pub fn routes() -> axum::Router {
    api::routes()
}
