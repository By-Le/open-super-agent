//! 配置：config.yaml（OSA_CONFIG 可以换），数据目录（OSA_DATA_DIR），数据库路径。对应 osa/config.py。
//!
//! CFG 运行中会被改（settings.apply 把界面上改过的盖上去），所以放在 RwLock 里；读用 `get("limits.worker_max_steps")`。

use parking_lot::RwLock;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// 仓库根目录（Python 版的 ROOT）：OSA_ROOT，没有就是当前目录。config.yaml、osa/static/dist 都相对它找
pub static ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var_os("OSA_ROOT").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
});

pub static CFG: LazyLock<RwLock<Value>> = LazyLock::new(|| RwLock::new(load()));

/// config.yaml 原样（启动时读的那份，不含 settings 盖上去的）
pub static ORIGINAL: LazyLock<Value> = LazyLock::new(load);

fn config_path() -> PathBuf {
    let name = std::env::var("OSA_CONFIG").unwrap_or_else(|_| "config.yaml".into());
    ROOT.join(name)
}

fn load() -> Value {
    let p = config_path();
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不了配置 {}：{e}", p.display()));
    let y: serde_yaml::Value = serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("配置 {} 格式不对：{e}", p.display()));
    yaml_to_json(y)
}

pub fn config_file() -> PathBuf {
    std::fs::canonicalize(config_path()).unwrap_or_else(|_| config_path())
}

pub fn yaml_to_json(y: serde_yaml::Value) -> Value {
    use serde_yaml::Value as Y;
    match y {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                json!(i)
            } else if let Some(u) = n.as_u64() {
                json!(u)
            } else {
                json!(n.as_f64().unwrap_or(0.0))
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(s) => Value::Array(s.into_iter().map(yaml_to_json).collect()),
        Y::Mapping(m) => {
            let mut o = serde_json::Map::new();
            for (k, v) in m {
                let key = match k {
                    Y::String(s) => s,
                    Y::Number(n) => n.to_string(),
                    Y::Bool(b) => {
                        if b {
                            "True".into()
                        } else {
                            "False".into()
                        }
                    }
                    other => serde_yaml::to_string(&other).unwrap_or_default().trim().to_string(),
                };
                o.insert(key, yaml_to_json(v));
            }
            Value::Object(o)
        }
        Y::Tagged(t) => yaml_to_json(t.value),
    }
}

/// 数据目录：数据库、密钥、自己加的应用清单、工作目录都在这里，默认是仓库里的 data/
pub static DATA: LazyLock<PathBuf> = LazyLock::new(|| match std::env::var_os("OSA_DATA_DIR") {
    Some(d) if !d.is_empty() => expand_user(Path::new(&d)),
    _ => ROOT.join("data"),
});

pub fn data_dir_set() -> bool {
    std::env::var_os("OSA_DATA_DIR").is_some_and(|d| !d.is_empty())
}

pub static DB_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    let db = get_str("memory.db");
    let db = if db.is_empty() { "data/osa.db".to_string() } else { db };
    let p = PathBuf::from(&db);
    if p.is_absolute() {
        p
    } else if data_dir_set() {
        DATA.join(p.file_name().unwrap_or_default())
    } else {
        ROOT.join(p)
    }
});

pub fn expand_user(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~")
        && let Some(h) = std::env::var_os("HOME")
    {
        return PathBuf::from(h).join(rest);
    }
    p.to_path_buf()
}

/// 按点分的路径取一项（找不到是 Null）
pub fn get(path: &str) -> Value {
    let c = CFG.read();
    let mut v = &*c;
    for k in path.split('.') {
        match v.get(k) {
            Some(x) => v = x,
            None => return Value::Null,
        }
    }
    v.clone()
}

pub fn get_str(path: &str) -> String {
    get(path).as_str().unwrap_or_default().to_string()
}

pub fn get_i64(path: &str, default: i64) -> i64 {
    let v = get(path);
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(default)
}

pub fn get_f64(path: &str, default: f64) -> f64 {
    get(path).as_f64().unwrap_or(default)
}

pub fn get_bool(path: &str, default: bool) -> bool {
    match get(path) {
        Value::Bool(b) => b,
        Value::Null => default,
        v => crate::py::truthy(&v),
    }
}

/// 改 CFG：拿到可写的整棵树
pub fn with_mut<R>(f: impl FnOnce(&mut Value) -> R) -> R {
    f(&mut CFG.write())
}

/// 某个角色（router/worker/reviewer/filter）的 (服务名, 模型名)
pub fn role(role: &str) -> (String, String) {
    let r = get(&format!("roles.{role}"));
    (r["provider"].as_str().unwrap_or_default().to_string(), r["model"].as_str().unwrap_or_default().to_string())
}

/// 某个角色调用模型时的额外参数：thinking: false、reasoning_effort
pub fn call_kwargs(role: &str) -> serde_json::Map<String, Value> {
    let r = get(&format!("roles.{role}"));
    let mut kw = serde_json::Map::new();
    if r.get("thinking") == Some(&Value::Bool(false)) {
        kw.insert("thinking".into(), json!({"type": "disabled"}));
    }
    if let Some(e) = r.get("reasoning_effort").filter(|e| crate::py::truthy(e)) {
        kw.insert("reasoning_effort".into(), e.clone());
    }
    kw
}
