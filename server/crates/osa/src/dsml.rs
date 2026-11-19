//! DeepSeek 偶尔不走正常的 tool_calls，把工具调用写进正文（DSML 标记）。认出来就当成真的工具调用去做，标记本身不给用户看。
//! 对应 osa/dsml.py。

use fancy_regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

fn tag(close: &str, name: &str) -> String {
    format!(r"<\s*{close}[｜|]+\s*DSML\s*[｜|]+\s*{name}")
}

static START: LazyLock<Regex> = LazyLock::new(|| Regex::new(&tag(r"/?\s*", "")).unwrap());
static INVOKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?s){}(.*?)(?={}|{}|\z)"#,
        tag("", r#"invoke\s+name\s*=\s*"([^"]+)"\s*>"#),
        tag(r"/\s*", r"invoke\s*>"),
        tag("", "invoke")
    ))
    .unwrap()
});
static PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?s){}(.*?){}"#,
        tag("", r#"parameter\s+name\s*=\s*"([^"]+)"(?:\s+string\s*=\s*"(true|false)")?\s*>"#),
        tag(r"/\s*", r"parameter\s*>")
    ))
    .unwrap()
});
static LAST: LazyLock<Regex> = LazyLock::new(|| Regex::new(&tag(r"/?\s*", r"[^>]*>")).unwrap());
static PARTIAL: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^<\s*/?\s*[｜|]*\s*(?:D(?:S(?:M(?:L)?)?)?)?\s*$").unwrap());
static CLOSE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^<\s*/").unwrap());

/// 边收边显示时，text 里前多少个字节可以先给用户看
pub fn visible(text: &str) -> usize {
    if let Ok(Some(m)) = START.find(text) {
        return m.start();
    }
    match text.rfind('<') {
        Some(i) if PARTIAL.is_match(&text[i..]) => i,
        _ => text.len(),
    }
}

fn value(raw: &str, is_string: Option<&str>) -> Value {
    if is_string == Some("false")
        && let Ok(v) = serde_json::from_str::<Value>(raw)
    {
        return v;
    }
    Value::String(raw.to_string())
}

/// 一个从正文里认出来的工具调用：和流式收到的一样，args 是 JSON 字符串
#[derive(Clone, Debug, PartialEq)]
pub struct Leaked {
    pub id: String,
    pub name: String,
    pub args: String,
}

/// 返回 (去掉标记后的正文, 工具调用)
pub fn split(text: &str) -> (String, Vec<Leaked>) {
    let Ok(Some(m)) = START.find(text) else {
        return (text.to_string(), vec![]);
    };
    let start = m.start();
    let tags: Vec<_> = LAST.find_iter(&text[start..]).filter_map(|x| x.ok()).collect();
    let rest = match tags.last() {
        Some(t) if CLOSE.is_match(t.as_str()) => &text[start + t.end()..],
        _ => "",
    };
    let mut calls = vec![];
    for (k, c) in INVOKE.captures_iter(&text[start..]).filter_map(|c| c.ok()).enumerate() {
        let name = c.get(1).map(|x| x.as_str()).unwrap_or("").trim().to_string();
        let body = c.get(2).map(|x| x.as_str()).unwrap_or("");
        let mut args = Map::new();
        for p in PARAM.captures_iter(body).filter_map(|p| p.ok()) {
            let key = p.get(1).map(|x| x.as_str()).unwrap_or("").to_string();
            let s = p.get(2).map(|x| x.as_str());
            let v = p.get(3).map(|x| x.as_str()).unwrap_or("");
            args.insert(key, value(v, s));
        }
        calls.push(Leaked { id: format!("call_dsml_{k}"), name, args: crate::py::dumps_u(&Value::Object(args)) });
    }
    let r = rest.trim();
    let clean = format!("{}{}", text[..start].trim_end(), if r.is_empty() { String::new() } else { format!("\n\n{r}") });
    (clean.trim().to_string(), calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaked_call() {
        let t = "好的\n<｜｜DSML｜｜ invoke name=\"message_bot\">\n<｜｜DSML｜｜ parameter name=\"to\" string=\"true\">Quark</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>";
        let (clean, calls) = split(t);
        assert_eq!(clean, "好的");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "message_bot");
        assert_eq!(calls[0].args, r#"{"to": "Quark"}"#);
        assert_eq!(visible("你好<｜"), "你好".len());
        assert_eq!(visible("你好"), "你好".len());
    }
}
