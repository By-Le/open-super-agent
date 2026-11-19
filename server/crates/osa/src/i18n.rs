//! 界面语言。中文原文就是翻译的键：t("没有这个 Bot") 在英文界面返回 locales/en.json 里对应的英文。对应 osa/i18n.py。

use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const LANGS: [&str; 2] = ["zh", "en"];

static EN: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../osa/locales/en.json")).expect("en.json")
});

static LANG: Mutex<Option<String>> = Mutex::new(None);

/// 设过的语言；没设过返回空
pub fn chosen() -> String {
    let mut l = LANG.lock();
    if l.is_none() {
        let v = crate::memory::setting("language");
        *l = Some(v.and_then(|v| v.as_str().map(String::from)).unwrap_or_default());
    }
    l.clone().unwrap_or_default()
}

pub fn lang() -> String {
    let c = chosen();
    if c.is_empty() { "zh".into() } else { c }
}

pub fn set_lang(value: &str) -> Result<(), String> {
    if !LANGS.contains(&value) {
        return Err(t("不支持这种语言：{lang}", &[("lang", value.into())]));
    }
    crate::memory::set_setting("language", &Value::String(value.into()));
    *LANG.lock() = Some(value.into());
    Ok(())
}

/// 恢复备份、测试换库后重新读
pub fn forget() {
    *LANG.lock() = None;
}

/// 按浏览器的 Accept-Language 猜：排在最前面的是中文就用中文，否则用英文
pub fn from_header(accept: Option<&str>) -> String {
    let first = accept.unwrap_or("").split(',').next().unwrap_or("").trim().to_lowercase();
    if first.is_empty() || first.starts_with("zh") { "zh".into() } else { "en".into() }
}

static PH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{(\w+)\}").unwrap());

/// 翻译用的变量
pub type Kw<'a> = &'a [(&'a str, Value)];

pub fn fill(s: &str, kw: Kw) -> String {
    if kw.is_empty() {
        return s.to_string();
    }
    let map: HashMap<&str, &Value> = kw.iter().map(|(k, v)| (*k, v)).collect();
    PH.replace_all(s, |c: &regex::Captures| match map.get(&c[1]) {
        Some(v) => crate::py::str_of(v),
        None => c[0].to_string(),
    })
    .into_owned()
}

/// 翻译一句话，变量写成 {名字}。英文可以写成 ["{n} task", "{n} tasks"]，按 n 是不是 1 选
pub fn t(zh: &str, kw: Kw) -> String {
    t_in(zh, None, kw)
}

pub fn t0(zh: &str) -> String {
    t_in(zh, None, &[])
}

pub fn t_in(zh: &str, in_lang: Option<&str>, kw: Kw) -> String {
    let mut s = zh.to_string();
    let l = match in_lang {
        Some(l) => l.to_string(),
        None => lang(),
    };
    if l == "en" {
        match EN.get(zh) {
            Some(Value::String(v)) if !v.is_empty() => s = v.clone(),
            Some(Value::Array(a)) => {
                let one = kw.iter().any(|(k, v)| *k == "n" && crate::py::as_int(v) == Some(1));
                if let Some(Value::String(v)) = a.get(if one { 0 } else { 1 }) {
                    if !v.is_empty() {
                        s = v.clone();
                    }
                }
            }
            _ => {}
        }
    }
    fill(&s, kw)
}

/// 只标记、不翻译（Python 的 N_）
pub fn n_(zh: &str) -> &str {
    zh
}

/// 写给模型的一句话：英文界面时默认用英文回复。中文界面什么都不加
pub fn model_hint() -> &'static str {
    if lang() == "zh" {
        ""
    } else {
        "\n用户的界面语言是英文：默认用英文回复，结果、页面和记下的记忆也用英文；用户用别的语言说话时跟着用那种语言。"
    }
}

/// 一句话的中英文两种说法
pub fn both(zh: &str) -> Vec<String> {
    let mut out = vec![zh.to_string()];
    match EN.get(zh) {
        Some(Value::String(s)) => out.push(s.clone()),
        Some(Value::Array(a)) => out.extend(a.iter().filter_map(|x| x.as_str().map(String::from))),
        _ => {}
    }
    out
}

/// 存起来、显示时才翻的说明（Python 的 Later）
#[derive(Clone, Debug, PartialEq)]
pub struct Later {
    pub zh: String,
    pub kw: Vec<(String, Value)>,
}

impl Later {
    pub fn new(zh: &str, kw: Kw) -> Self {
        Later { zh: zh.into(), kw: kw.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }
    }
    pub fn plain(zh: &str) -> Self {
        Later { zh: zh.into(), kw: vec![] }
    }
    pub fn text(&self) -> String {
        let kw: Vec<(&str, Value)> = self.kw.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        t(&self.zh, &kw)
    }
}

impl std::fmt::Display for Later {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

/// t("…", &[("n", json!(3))]) 的简写
#[macro_export]
macro_rules! t {
    ($zh:expr) => { $crate::i18n::t0($zh) };
    ($zh:expr, $($k:ident = $v:expr),+ $(,)?) => {
        $crate::i18n::t($zh, &[$((stringify!($k), serde_json::json!($v))),+])
    };
}
