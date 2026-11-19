//! 一个后台任务（Python 版 worker.py 的 Task）。不变的字段直接放，会变的放在 st 里。

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 私信的回信去向（见 dm.rs）
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub bot: i64,
    pub chat: i64,
    pub depth: i64,
    pub then: Option<Box<Reply>>,
}

#[derive(Clone, Debug)]
pub struct Handoff {
    pub to: i64,
    pub note: String,
}

pub type OnDone = Box<dyn FnOnce(Arc<Task>) -> futures::future::BoxFuture<'static, ()> + Send>;

pub struct TaskState {
    pub status: String,
    pub started: f64,
    pub last_progress: f64,
    pub long_calls: HashMap<u64, f64>,
    pub steps: i64,
    pub recent_calls: Vec<String>,
    pub waiting_approval: Option<i64>,
    pub paused_s: f64,
    pub last_action: String,
    pub result: String,
    pub handoff: Option<Handoff>,
    pub reply: Option<Reply>,
    pub quiet: bool,
    pub files: Vec<Value>,
    pub apps: Vec<Value>,
}

pub struct Task {
    pub id: i64,
    /// 冒充的任务（对话里直接操作电脑、画图、应用界面调用）：id 是文字
    pub fake_id: Option<String>,
    pub goal: String,
    pub mode: String,
    pub bot_id: i64,
    pub chat_id: i64,
    pub approval_chat_id: i64,
    pub background: bool,
    pub depth: i64,
    pub quiet_ok: bool,
    pub notes: String,
    pub mac: i64,
    pub mac_name: String,
    pub st: Mutex<TaskState>,
    pub cancel: CancellationToken,
    pub on_done: Mutex<Option<OnDone>>,
    pub files: Arc<Mutex<Vec<Value>>>,
    pub apps: Arc<Mutex<Vec<Value>>>,
}

#[derive(Default)]
pub struct NewTask {
    pub id: i64,
    pub fake_id: Option<String>,
    pub goal: String,
    pub mode: String,
    pub bot_id: i64,
    pub chat_id: i64,
    pub approval_chat_id: i64,
    pub background: bool,
    pub handoff: Option<Handoff>,
    pub reply: Option<Reply>,
    pub depth: i64,
    pub quiet_ok: bool,
    pub notes: String,
    pub mac: i64,
    pub mac_name: String,
}

impl Task {
    pub fn new(n: NewTask) -> Arc<Task> {
        let now = crate::py::now();
        Arc::new(Task {
            id: n.id,
            fake_id: n.fake_id,
            goal: n.goal,
            mode: if n.mode.is_empty() { "single".into() } else { n.mode },
            bot_id: n.bot_id,
            chat_id: n.chat_id,
            approval_chat_id: n.approval_chat_id,
            background: n.background,
            depth: n.depth,
            quiet_ok: n.quiet_ok,
            notes: n.notes,
            mac: n.mac,
            mac_name: n.mac_name,
            st: Mutex::new(TaskState {
                status: "running".into(),
                started: now,
                last_progress: now,
                long_calls: HashMap::new(),
                steps: 0,
                recent_calls: vec![],
                waiting_approval: None,
                paused_s: 0.0,
                last_action: String::new(),
                result: String::new(),
                handoff: n.handoff,
                reply: n.reply,
                quiet: false,
                files: vec![],
                apps: vec![],
            }),
            cancel: CancellationToken::new(),
            on_done: Mutex::new(None),
            files: Arc::new(Mutex::new(vec![])),
            apps: Arc::new(Mutex::new(vec![])),
        })
    }

    /// 冒充的任务（Python 的 SimpleNamespace）
    pub fn fake(id: &str, bot_id: i64, chat_id: i64, goal: &str, background: bool) -> Arc<Task> {
        Task::new(NewTask {
            fake_id: Some(id.to_string()),
            goal: goal.into(),
            bot_id,
            chat_id,
            approval_chat_id: chat_id,
            background,
            ..Default::default()
        })
    }

    pub fn id_value(&self) -> Value {
        match &self.fake_id {
            Some(s) => json!(s),
            None => json!(self.id),
        }
    }

    pub fn status(&self) -> String {
        self.st.lock().status.clone()
    }

    pub fn touch(&self) {
        self.st.lock().last_progress = crate::py::now();
    }

    pub fn public(&self) -> Value {
        let s = self.st.lock();
        let mut o = json!({"id": self.id_value(), "goal": self.goal, "mode": self.mode, "status": s.status, "steps": s.steps,
            "bot_id": self.bot_id, "chat_id": self.chat_id, "started": s.started,
            "waiting_approval": s.waiting_approval, "last_action": s.last_action,
            "elapsed": crate::py::round0(crate::py::now() - s.started), "quiet": s.quiet});
        if self.mac != 0 {
            o["mac"] = json!(self.mac_name);
        }
        o
    }
}
