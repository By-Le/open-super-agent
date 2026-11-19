//! 照着 Python 的行为写的小工具：真假值、json.dumps 的格式、time.time()、按字符截断。
//! 存进数据库、写进提示词的 JSON 要和 Python 版逐字一样（", " 和 ": "），换回 Python 版、模型服务的缓存都靠它。

use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Python 的 bool(x)
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// time.time()
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// time.time_ns()
pub fn now_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Python 的 round(x)（整数，银行家舍入）
pub fn round0(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 { (r - x.signum()) as i64 } else { r as i64 }
}

/// Python 的 round(x, n)
pub fn round_n(x: f64, n: i32) -> f64 {
    let m = 10f64.powi(n);
    let y = x * m;
    let r = y.round();
    let r = if (y - y.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 { r - y.signum() } else { r };
    r / m
}

/// json.dumps(v)（ensure_ascii=False 时 ascii=false）
pub fn dumps(v: &Value, ascii: bool) -> String {
    let mut out = String::new();
    write(&mut out, v, ascii);
    out
}

/// json.dumps(v, ensure_ascii=False)
pub fn dumps_u(v: &Value) -> String {
    dumps(v, false)
}

fn write(out: &mut String, v: &Value, ascii: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&num(n)),
        Value::String(s) => string(out, s, ascii),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(out, x, ascii);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                string(out, k, ascii);
                out.push_str(": ");
                write(out, x, ascii);
            }
            out.push('}');
        }
    }
}

/// Python 的 float repr：整数值的浮点数写成 1.0
fn num(n: &serde_json::Number) -> String {
    if n.is_f64() {
        let f = n.as_f64().unwrap_or(0.0);
        if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
            format!("{f:.1}")
        } else {
            let s = f.to_string();
            // Rust 的 1e20 写成 100000000000000000000；Python 是 1e+20，很少见，按 Rust 的写
            s
        }
    } else {
        n.to_string()
    }
}

fn string(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// s[:n]（按字符）
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// s[-n:]（按字符）
pub fn tail(s: &str, n: usize) -> String {
    let len = s.chars().count();
    s.chars().skip(len.saturating_sub(n)).collect()
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// json.dumps(v, ensure_ascii=False, indent=n)
pub fn dumps_indent(v: &Value, indent: usize, ascii: bool) -> String {
    let mut out = String::new();
    write_indent(&mut out, v, indent, 0, ascii);
    out
}

fn write_indent(out: &mut String, v: &Value, indent: usize, level: usize, ascii: bool) {
    match v {
        Value::Array(a) if !a.is_empty() => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&" ".repeat(indent * (level + 1)));
                write_indent(out, x, indent, level + 1, ascii);
            }
            out.push('\n');
            out.push_str(&" ".repeat(indent * level));
            out.push(']');
        }
        Value::Object(o) if !o.is_empty() => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&" ".repeat(indent * (level + 1)));
                string(out, k, ascii);
                out.push_str(": ");
                write_indent(out, x, indent, level + 1, ascii);
            }
            out.push('\n');
            out.push_str(&" ".repeat(indent * level));
            out.push('}');
        }
        other => write(out, other, ascii),
    }
}

/// 本地时间的 strftime（Python 的 time.strftime）
pub fn strftime(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

pub fn strftime_at(ts: f64, fmt: &str) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts.floor() as i64, ((ts.fract()) * 1e9) as u32) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.format(fmt).to_string(),
        chrono::LocalResult::None => String::new(),
    }
}

/// dict 合并（a | b）
pub fn merge(a: &Value, b: &Value) -> Value {
    let mut o = a.as_object().cloned().unwrap_or_default();
    if let Some(b) = b.as_object() {
        for (k, v) in b {
            o.insert(k.clone(), v.clone());
        }
    }
    Value::Object(o)
}

/// 从 JSON 里取 i64（int 或整数值的 float；Python 里 bool 也是 int，这里不认）
pub fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        _ => None,
    }
}

/// int(x)：数字或数字字符串
pub fn to_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// str(x) 用在提示词里：字符串原样，别的按 Python 的写法
pub fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => num(n),
        other => repr(other),
    }
}

/// repr(dict/list)：Python 的写法，字符串用单引号
pub fn repr(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
            let mut out = String::from(q);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == q => {
                        out.push('\\');
                        out.push(c)
                    }
                    c => out.push(c),
                }
            }
            out.push(q);
            out
        }
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!("{{{}}}", o.iter().map(|(k, x)| format!("{}: {}", repr(&Value::String(k.clone())), repr(x))).collect::<Vec<_>>().join(", ")),
        other => str_of(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_like_python() {
        assert_eq!(dumps(&json!({"a": 1, "b": [1, 2.0, "中"], "c": null}), false), r#"{"a": 1, "b": [1, 2.0, "中"], "c": null}"#);
        assert_eq!(dumps(&json!("中\n"), true), r#""中\n""#);
        assert_eq!(round0(2.5), 2);
        assert_eq!(round0(3.5), 4);
        assert_eq!(round_n(1.23456, 1), 1.2);
    }
}
