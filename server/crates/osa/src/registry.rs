//! 工具注册表：Router、Worker、小应用都从这里找工具；事件 on / emit；事件来源。对应 osa/registry.py。

use crate::task::Task;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::sync::{Arc, LazyLock};

pub const ROUTER: &str = "router";
pub const WORKER: &str = "worker";
pub const APPLET: &str = "applet";

/// 往对话里写一条消息：(role, content, chat_id, meta) -> 消息 id
pub type PostFn = Arc<dyn Fn(String, String, i64, Value) -> BoxFuture<'static, i64> + Send + Sync>;

/// 工具返回：给模型的文字，可以带一张截图（png）
pub enum Out {
    Text(String),
    Shot(String, Option<Vec<u8>>),
}

impl From<String> for Out {
    fn from(s: String) -> Self {
        Out::Text(s)
    }
}
impl From<&str> for Out {
    fn from(s: &str) -> Self {
        Out::Text(s.to_string())
    }
}

/// 一次工具调用的上下文
#[derive(Clone)]
pub struct Call {
    pub scope: &'static str,
    pub bot: Value,
    pub chat_id: i64,
    pub private_chat_id: i64,
    pub args: Value,
    pub task: Option<Arc<Task>>,
    pub post: Option<PostFn>,
    pub files: Arc<Mutex<Vec<Value>>>,
    pub apps: Arc<Mutex<Vec<Value>>>,
    pub env: Value,
}

impl Call {
    pub fn bot_id(&self) -> i64 {
        self.bot["id"].as_i64().unwrap_or(0)
    }
    pub fn arg_str(&self, k: &str) -> String {
        match &self.args[k] {
            Value::Null => String::new(),
            v => crate::py::str_of(v),
        }
    }
}

pub type RunFn = Arc<dyn Fn(Call) -> BoxFuture<'static, Out> + Send + Sync>;

pub struct Tool {
    pub name: String,
    pub schema: Value,
    pub run: RunFn,
    pub where_: Vec<&'static str>,
    pub when: Option<fn(&Value) -> bool>,
    pub gated: bool,
    pub parallel: usize,
    pub progress: Option<Arc<dyn Fn(&Value) -> String + Send + Sync>>,
    pub describe: String,
    pub group: String,
    pub meta: Value,
    pub owner: String,
}

impl Tool {
    pub fn new(name: &str, schema: Value, run: RunFn) -> Tool {
        Tool {
            name: name.into(),
            schema,
            run,
            where_: vec![ROUTER, WORKER],
            when: None,
            gated: false,
            parallel: 0,
            progress: None,
            describe: String::new(),
            group: String::new(),
            meta: Value::Null,
            owner: "builtin".into(),
        }
    }
    pub fn offered(&self, scope: &str, env: &Value) -> bool {
        self.where_.contains(&scope) && self.when.is_none_or(|w| w(env))
    }
}

pub static TOOLS: LazyLock<RwLock<IndexMap<String, Arc<Tool>>>> = LazyLock::new(Default::default);

type Listener = Arc<dyn Fn(Value) -> BoxFuture<'static, ()> + Send + Sync>;
static LISTENERS: LazyLock<Mutex<IndexMap<String, Vec<(u64, Listener)>>>> = LazyLock::new(Default::default);
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 注册用的句柄：dispose() 反向撤销全部注册
pub struct Context {
    pub name: String,
    disposers: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl Context {
    pub fn new(name: &str) -> Context {
        Context { name: name.into(), disposers: Mutex::new(vec![]) }
    }

    pub fn tool(&self, mut tool: Tool) -> Result<(), String> {
        if tool.gated && tool.where_.contains(&ROUTER) {
            return Err(format!("工具 {} 标了 gated（跑之前要用户确认），不能给 ROUTER", tool.name));
        }
        tool.owner = self.name.clone();
        let mut tools = TOOLS.write();
        if let Some(old) = tools.get(&tool.name) {
            if old.owner != self.name {
                return Err(format!("工具 {} 已经由 {} 注册了，换个名字", tool.name, old.owner));
            }
        }
        let name = tool.name.clone();
        let t = Arc::new(tool);
        let me = t.clone();
        // Python 的 dict 覆盖同名键时位置不变
        tools.insert(name.clone(), t);
        drop(tools);
        self.disposers.lock().push(Box::new(move || {
            let mut tools = TOOLS.write();
            if tools.get(&name).is_some_and(|x| Arc::ptr_eq(x, &me)) {
                tools.shift_remove(&name);
            }
        }));
        Ok(())
    }

    pub fn on(&self, event: &str, f: Listener) {
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        LISTENERS.lock().entry(event.to_string()).or_default().push((id, f));
        let ev = event.to_string();
        self.disposers.lock().push(Box::new(move || {
            if let Some(v) = LISTENERS.lock().get_mut(&ev) {
                v.retain(|(i, _)| *i != id);
            }
        }));
    }

    pub fn dispose(&self) {
        loop {
            let d = self.disposers.lock().pop();
            match d {
                Some(f) => f(),
                None => break,
            }
        }
    }
}

pub fn get(name: &str) -> Option<Arc<Tool>> {
    TOOLS.read().get(name).cloned()
}

pub fn available(name: &str, scope: &str, env: &Value) -> Option<Arc<Tool>> {
    get(name).filter(|t| t.offered(scope, env))
}

pub fn schemas(scope: &str, env: &Value) -> Vec<Value> {
    TOOLS.read().values().filter(|t| t.offered(scope, env)).map(|t| t.schema.clone()).collect()
}

pub fn all() -> Vec<Arc<Tool>> {
    TOOLS.read().values().cloned().collect()
}

/// 按应用分组的 MCP 工具（group 非空的），按注册顺序
pub fn by_group() -> IndexMap<String, Vec<Arc<Tool>>> {
    let mut out: IndexMap<String, Vec<Arc<Tool>>> = IndexMap::new();
    for t in TOOLS.read().values() {
        if !t.group.is_empty() {
            out.entry(t.group.clone()).or_default().push(t.clone());
        }
    }
    out
}

/// 发事件但不等监听器跑完
pub fn fire(event: &str, kw: Value) {
    let ls: Vec<Listener> = LISTENERS.lock().get(event).map(|v| v.iter().map(|(_, f)| f.clone()).collect()).unwrap_or_default();
    if ls.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for f in ls {
            f(kw.clone()).await;
        }
    });
}

pub async fn emit(event: &str, kw: Value) {
    let ls: Vec<Listener> = LISTENERS.lock().get(event).map(|v| v.iter().map(|(_, f)| f.clone()).collect()).unwrap_or_default();
    for f in ls {
        f(kw.clone()).await;
    }
}

/// 工具实现的简写：async 闭包 → RunFn
#[macro_export]
macro_rules! run_fn {
    (|$c:ident| $body:expr) => {
        std::sync::Arc::new(move |$c: $crate::registry::Call| -> futures::future::BoxFuture<'static, $crate::registry::Out> {
            Box::pin(async move { $body })
        })
    };
}
