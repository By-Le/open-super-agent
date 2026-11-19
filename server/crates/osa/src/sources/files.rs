//! 文件夹里有新文件：下载、截图、发票、共享文件夹……只在 security.local_exec 的地方能用。对应 osa/sources/files.py。

use crate::registry::Source;
use crate::util::fnmatch;
use crate::{config, py, security};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const MAX_FILES: usize = 5000;
pub const MAX_SCAN: usize = 50_000;
pub const MAX_REPORT: usize = 30;
const PARTIAL: [&str; 15] = [
    ".crdownload", ".part", ".partial", ".download", ".downloading", ".tmp", ".temp", ".opdownload", ".td", ".td.cfg", ".aria2", ".!qb",
    ".xltd", ".bc!", ".filepart",
];

pub fn unavailable() -> String {
    if !security::local_exec() {
        return t!("这台服务器开了服务器锁，不能看服务器上的文件夹（config.yaml 的 security.local_exec）");
    }
    String::new()
}

pub fn patterns(p: &Value) -> Vec<String> {
    static SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[,，;；\n]+").unwrap());
    let raw = if py::truthy(&p["pattern"]) { py::str_of(&p["pattern"]) } else { "*".into() };
    let out: Vec<String> = SEP.split(&raw).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    if out.is_empty() { vec!["*".into()] } else { out }
}

fn skip(name: &str) -> bool {
    let low = name.to_lowercase();
    name.starts_with('.') || name.starts_with("~$") || PARTIAL.iter().any(|p| low.ends_with(p)) || low == "thumbs.db" || low == "desktop.ini"
}

/// Python 的 round(x, 3)
fn round3(x: f64) -> f64 {
    py::round_n(x, 3)
}

/// {相对路径: [大小, 修改时间]}，只要符合 pattern 的文件；超过 MAX_FILES 个只留最新的
pub fn scan(root: &Path, patterns: &[String], recursive: bool) -> Result<Map<String, Value>, String> {
    let mut found = Map::new();
    let mut seen = 0usize;
    let mut stack = vec![String::new()];
    let pats: Vec<String> = patterns.iter().map(|x| x.to_lowercase()).collect();
    'outer: while let Some(rel) = stack.pop() {
        if seen >= MAX_SCAN {
            break;
        }
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let it = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                if rel.is_empty() {
                    return Err(format!(
                        "没有权限看这个文件夹：{}。macOS 上要在“系统设置 → 隐私与安全性 → 文件与文件夹”里允许 osa",
                        root.display()
                    ));
                }
                continue;
            }
            Err(_) => continue,
        };
        for e in it.flatten() {
            seen += 1;
            if seen > MAX_SCAN {
                break 'outer;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if skip(&name) {
                continue;
            }
            let sub = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if recursive {
                    stack.push(sub);
                }
                continue;
            }
            // 符号链接指向文件的也算（os.DirEntry.is_file 跟随链接）
            let Ok(md) = std::fs::metadata(e.path()) else { continue };
            if !md.is_file() {
                continue;
            }
            if !pats.iter().any(|p| fnmatch(&name.to_lowercase(), p)) {
                continue;
            }
            let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            found.insert(sub, json!([md.len(), round3(mtime)]));
        }
    }
    Ok(newest(found))
}

fn mtime_of(v: &Value) -> f64 {
    v[1].as_f64().unwrap_or(0.0)
}

/// 超过 MAX_FILES 个只留最新的（按修改时间从新到旧，和 Python 的 sorted(reverse=True) 一样稳定）
fn newest(m: Map<String, Value>) -> Map<String, Value> {
    if m.len() <= MAX_FILES {
        return m;
    }
    let mut items: Vec<(String, Value)> = m.into_iter().collect();
    items.sort_by(|a, b| mtime_of(&b.1).partial_cmp(&mtime_of(&a.1)).unwrap_or(std::cmp::Ordering::Equal));
    items.truncate(MAX_FILES);
    items.into_iter().collect()
}

fn size(n: u64) -> String {
    let mut x = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if x < 1024.0 || unit == "GB" {
            return if unit == "B" { format!("{} {unit}", super::util::f(x, 0)) } else { format!("{} {unit}", super::util::f(x, 1)) };
        }
        x /= 1024.0;
    }
    String::new()
}

fn event(root: &str, rel: &str, info: &Value, changed: bool) -> Value {
    let path = Path::new(root).join(rel).to_string_lossy().into_owned();
    let kind = mime_guess::from_path(rel).first().map(|m| m.essence_str().to_string()).unwrap_or_default();
    let when = py::strftime_at(mtime_of(info), "%Y-%m-%d %H:%M");
    let lines = [
        format!("位置：{path}"),
        format!("大小：{}", size(info[0].as_u64().unwrap_or(0))),
        format!("修改时间：{when}"),
        if kind.is_empty() { String::new() } else { format!("类型：{kind}") },
    ];
    json!({"id": format!("{rel}-{}", py::str_of(&info[1])), "title": format!("{}：{rel}", if changed { "文件改了" } else { "新文件" }),
           "text": lines.iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join("\n")})
}

/// 这次看到的 cur 和上次的 state 比：新文件（和改了的）先放进 pending，下一次还是一样大才报
pub fn compare(root: &str, cur: &Map<String, Value>, state: &Value, modified: bool) -> (Vec<Value>, Value) {
    if !state.is_object() {
        // 第一次只记下现状；最新的一个给“测试”当示例（max：一样新的取第一个）
        let mut best: Option<(&String, &Value)> = None;
        for (k, v) in cur {
            if best.is_none_or(|(_, b)| mtime_of(v) > mtime_of(b)) {
                best = Some((k, v));
            }
        }
        let ev = best.map(|(k, v)| vec![event(root, k, v, false)]).unwrap_or_default();
        return (ev, json!({"files": cur, "pending": {}}));
    }
    let mut files = state["files"].as_object().cloned().unwrap_or_default();
    let pending = state["pending"].as_object().cloned().unwrap_or_default();
    let mut report: Vec<(String, Value, bool)> = vec![];
    let mut nxt = Map::new();
    for (rel, info) in cur {
        let old = files.get(rel).cloned();
        if old.as_ref() == Some(info) {
            continue;
        }
        if old.is_some() && !modified {
            files.insert(rel.clone(), info.clone());
            continue;
        }
        if pending.get(rel) == Some(info) {
            report.push((rel.clone(), info.clone(), old.is_some()));
            files.insert(rel.clone(), info.clone());
        } else {
            nxt.insert(rel.clone(), info.clone());
        }
    }
    files.retain(|k, _| cur.contains_key(k));
    let files = newest(files);
    report.sort_by(|a, b| mtime_of(&a.1).partial_cmp(&mtime_of(&b.1)).unwrap_or(std::cmp::Ordering::Equal));
    let mut events: Vec<Value> = report.iter().take(MAX_REPORT).map(|(r, i, c)| event(root, r, i, *c)).collect();
    if report.len() > MAX_REPORT {
        let rest = &report[MAX_REPORT..];
        let names: Vec<String> = rest.iter().take(200).map(|(r, _, _)| r.clone()).collect();
        events.push(json!({"id": format!("more-{}", py::str_of(&rest[rest.len() - 1].1[1])), "title": format!("另外还有 {} 个文件", rest.len()),
                           "text": format!("{}{}", names.join("\n"), if rest.len() > 200 { "\n……" } else { "" })}));
    }
    (events, json!({"files": files, "pending": nxt}))
}

/// os.path.abspath(os.path.expanduser(path))
pub fn root_of(p: &Value) -> PathBuf {
    let raw = p["path"].as_str().unwrap_or("").trim().to_string();
    let e = config::expand_user(Path::new(&raw));
    let abs = if e.is_absolute() { e } else { std::env::current_dir().unwrap_or_default().join(e) };
    PathBuf::from(crate::util::normpath(&abs.to_string_lossy()))
}

pub async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let why = unavailable();
    if !why.is_empty() {
        return Err(why);
    }
    let root = root_of(&params);
    if !root.is_dir() {
        return Err(format!("文件夹不存在了：{}", root.display()));
    }
    let pats = patterns(&params);
    let rec = py::truthy(&params["recursive"]);
    let r2 = root.clone();
    let cur = tokio::task::spawn_blocking(move || scan(&r2, &pats, rec)).await.map_err(|e| e.to_string())??;
    Ok(compare(&root.to_string_lossy(), &cur, &state, py::truthy(&params["modified"])))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("folder");
    s.available = Some(unavailable);
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.validate = Some(|mut p| {
        if !py::truthy(&p["pattern"]) {
            p["pattern"] = json!("*");
        }
        let root = root_of(&p);
        if !root.is_dir() {
            return Err(format!("文件夹不存在：{}", root.display()));
        }
        Ok(p)
    });
    s.summary = Some(|p| {
        let pat = if py::truthy(&p["pattern"]) { py::str_of(&p["pattern"]) } else { "*".into() };
        let path = if py::truthy(&p["path"]) { py::str_of(&p["path"]) } else { String::new() };
        format!("{path}{}", if pat != "*" { format!("（{pat}）") } else { String::new() })
    });
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_scans_before_report() {
        let d = std::env::temp_dir().join(format!("osa-folder-{}", crate::util::token_hex(4)));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.pdf"), b"12345").unwrap();
        std::fs::write(d.join(".hidden"), b"x").unwrap();
        std::fs::write(d.join("b.crdownload"), b"x").unwrap();
        std::fs::write(d.join("sub/c.pdf"), b"x").unwrap();
        let root = d.to_string_lossy().into_owned();
        let pats = vec!["*.PDF".to_string()];
        let cur = scan(&d, &pats, false).unwrap();
        assert_eq!(cur.keys().cloned().collect::<Vec<_>>(), vec!["a.pdf"]);
        let (sample, st) = compare(&root, &cur, &Value::Null, false);
        assert_eq!(sample[0]["title"], "新文件：a.pdf");
        assert!(sample[0]["text"].as_str().unwrap().contains("大小：5 B") && sample[0]["text"].as_str().unwrap().contains("类型：application/pdf"));
        std::fs::write(d.join("new.pdf"), b"abc").unwrap();
        let cur = scan(&d, &pats, true).unwrap();
        assert!(cur.contains_key("sub/c.pdf"));
        let (ev, st) = compare(&root, &cur, &st, false);
        assert!(ev.is_empty());
        assert!(st["pending"].as_object().unwrap().contains_key("new.pdf"));
        let (ev, st) = compare(&root, &cur, &st, false);
        let titles: Vec<String> = ev.iter().map(|e| py::str_of(&e["title"])).collect();
        assert!(titles.contains(&"新文件：new.pdf".to_string()) && titles.contains(&"新文件：sub/c.pdf".to_string()));
        assert!(st["pending"].as_object().unwrap().is_empty());
        assert_eq!(size(1536), "1.5 KB");
        let _ = std::fs::remove_dir_all(&d);
    }
}
