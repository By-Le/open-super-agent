//! 网页上能做到什么程度（config.yaml 的 security）。对应 osa/security.py。

use crate::{catalog, config, mcp_hub, t};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::LazyLock;

pub fn workspace() -> PathBuf {
    config::DATA.join("workspace")
}

static SLOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\{\w+\})").unwrap());
static SLOT_FULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\{\w+\}$").unwrap());
static ENV_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^env:|\{env:)").unwrap());
pub const FS_PACKAGE: &str = "@modelcontextprotocol/server-filesystem";

fn cfg() -> Value {
    config::get("security")
}

fn levels(key: &str) -> &'static [&'static str] {
    if key == "exec" { &["off", "sandbox", "host"] } else { &["market", "any"] }
}

static WARNED: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Default::default);

fn level(key: &str, raw: &Value) -> String {
    let ls = levels(key);
    let v = if raw == &Value::Bool(false) { "off".to_string() } else { crate::py::str_of(raw).trim().to_lowercase() };
    if ls.contains(&v.as_str()) {
        return v;
    }
    if WARNED.lock().insert((key.into(), v.clone())) {
        tracing::warn!("config.yaml 的 security.{key} 写的是 {raw}，只能是 {}；按 {} 处理", ls.join(" / "), ls[0]);
    }
    ls[0].into()
}

pub fn policy() -> (String, String) {
    let c = cfg();
    let demo = crate::py::truthy(&c["demo"]);
    let (mut e, mut a) = if demo {
        ("sandbox".to_string(), "market".to_string())
    } else if c.get("local_exec").is_none_or(|v| v.is_null() || crate::py::truthy(v)) {
        ("host".into(), "any".into())
    } else {
        ("sandbox".into(), "market".into())
    };
    if !c["exec"].is_null() && c.get("exec").is_some() {
        e = level("exec", &c["exec"]);
    }
    if !c["apps"].is_null() && c.get("apps").is_some() {
        a = level("apps", &c["apps"]);
    }
    if demo {
        let idx = |k: &str, v: &str| levels(k).iter().position(|x| *x == v).unwrap_or(0);
        if idx("exec", &e) > idx("exec", "sandbox") {
            e = "sandbox".into();
        }
        if idx("apps", &a) > idx("apps", "market") {
            a = "market".into();
        }
    }
    (e, a)
}

pub fn exec_mode() -> String {
    policy().0
}

pub fn apps_mode() -> String {
    policy().1
}

pub fn demo() -> bool {
    crate::py::truthy(&cfg()["demo"])
}

pub fn local_exec() -> bool {
    exec_mode() == "host"
}

pub fn public() -> Value {
    let (e, a) = policy();
    json!({"local_exec": local_exec(), "demo": demo(), "exec": e, "apps": a})
}

pub fn no_local_exec() -> String {
    t!("这台服务器不允许从网页上在服务器本机执行命令（config.yaml 的 security.exec）")
}

fn market_only() -> String {
    t!("这台服务器上只能用应用市场里原样的本地应用，启动命令、参数和环境变量都不能改（config.yaml 的 security.apps）")
}

fn no_stdio_in_demo() -> String {
    t!("演示服务器上不能用在服务器上运行程序的应用，可以添加远程连接的应用")
}

fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

pub fn env_refs(cfg: &Value) -> bool {
    let mut all = vec![];
    for k in ["url", "headers", "args", "env"] {
        strings(&cfg[k], &mut all);
    }
    all.iter().any(|s| ENV_REF.is_match(s))
}

fn matches(tpl: &Value, value: &Value, whole_arg: bool) -> bool {
    match tpl {
        Value::String(t) => {
            let Some(v) = value.as_str() else { return false };
            if whole_arg && SLOT_FULL.is_match(t) && v.starts_with('-') {
                return false;
            }
            let mut rx = String::from("(?s)^");
            let mut last = 0;
            for m in SLOT.find_iter(t) {
                rx.push_str(&regex::escape(&t[last..m.start()]));
                rx.push_str("(.*)");
                last = m.end();
            }
            rx.push_str(&regex::escape(&t[last..]));
            rx.push('$');
            Regex::new(&rx).is_ok_and(|r| r.is_match(v))
        }
        Value::Array(ts) => value.as_array().is_some_and(|vs| ts.len() == vs.len() && ts.iter().zip(vs).all(|(t, v)| matches(t, v, true))),
        Value::Object(to) => value.as_object().is_some_and(|vo| {
            let mut a: Vec<_> = to.keys().collect();
            let mut b: Vec<_> = vo.keys().collect();
            a.sort();
            b.sort();
            a == b && to.iter().all(|(k, t)| matches(t, &vo[k], false))
        }),
        other => other == value,
    }
}

pub fn from_market(cfg: &Value) -> bool {
    let Some(it) = catalog::builtin(cfg["catalog"].as_str().unwrap_or("")) else { return false };
    let tpl = &it["connect"];
    let kind = tpl["type"].as_str().unwrap_or("stdio");
    if cfg["type"].as_str().filter(|s| !s.is_empty()).unwrap_or("stdio") != kind {
        return false;
    }
    let or_empty = |v: &Value, d: Value| if crate::py::truthy(v) { v.clone() } else { d };
    if kind == "stdio" {
        return tpl["command"] == cfg["command"]
            && matches(&or_empty(&tpl["args"], json!([])), &or_empty(&cfg["args"], json!([])), false)
            && matches(&or_empty(&tpl["env"], json!({})), &or_empty(&cfg["env"], json!({})), false);
    }
    matches(&tpl["url"], &or_empty(&cfg["url"], json!("")), false) && matches(&or_empty(&tpl["headers"], json!({})), &or_empty(&cfg["headers"], json!({})), false)
}

fn connect_of(c: &Value) -> Value {
    let or = |v: &Value, d: Value| if crate::py::truthy(v) { v.clone() } else { d };
    json!({"type": or(&c["type"], json!("stdio")), "command": or(&c["command"], json!("")), "args": or(&c["args"], json!([])),
           "env": or(&c["env"], json!({})), "url": or(&c["url"], json!("")), "headers": or(&c["headers"], json!({}))})
}

fn configured(name: &str, cfg: &Value) -> bool {
    let s = config::get("mcp_servers")[name].clone();
    crate::py::truthy(&s) && connect_of(&mcp_hub::from_yaml(&s)) == connect_of(cfg)
}

fn inside_workspace(cfg: &Value) -> Result<(), String> {
    let args: Vec<String> = cfg["args"].as_array().map(|a| a.iter().map(crate::py::str_of).collect()).unwrap_or_default();
    let Some(i) = args.iter().position(|a| a == FS_PACKAGE) else { return Ok(()) };
    let root = std::fs::canonicalize(workspace()).unwrap_or_else(|_| workspace());
    for a in &args[i + 1..] {
        let p = config::expand_user(std::path::Path::new(a));
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        if p != root && !p.starts_with(&root) {
            return Err(t!("这台服务器上，文件夹应用只能访问工作文件夹（{dir}）", dir = root.display().to_string()));
        }
    }
    Ok(())
}

pub fn check_app(name: &str, cfg: &Value) -> Result<(), String> {
    if apps_mode() == "any" || configured(name, cfg) {
        return Ok(());
    }
    let stdio = cfg["type"].as_str().filter(|s| !s.is_empty()).unwrap_or("stdio") == "stdio";
    if demo() {
        if stdio {
            return Err(no_stdio_in_demo());
        }
        if !from_market(cfg) {
            return Err(t!("演示服务器上只能添加应用市场里的应用"));
        }
    }
    if env_refs(cfg) {
        return Err(t!("这台服务器上，应用配置不能引用服务的环境变量（env:名字）"));
    }
    if stdio {
        if !from_market(cfg) {
            return Err(market_only());
        }
        inside_workspace(cfg)?;
    }
    Ok(())
}

pub fn check_market(item_id: &str) -> Result<(), String> {
    let Some(it) = catalog::items().get(item_id).cloned() else { return Ok(()) };
    if apps_mode() == "any" || it["connect"]["type"].as_str().unwrap_or("stdio") != "stdio" {
        return Ok(());
    }
    if demo() {
        return Err(no_stdio_in_demo());
    }
    if catalog::builtin(item_id).is_none() {
        return Err(market_only());
    }
    Ok(())
}

pub async fn check_provider_url(url: &str) -> Result<(), String> {
    if !demo() {
        return Ok(());
    }
    if crate::tools::private_host(url).await {
        return Err(t!("演示服务器上不能用本机或内网的模型服务地址"));
    }
    Ok(())
}
