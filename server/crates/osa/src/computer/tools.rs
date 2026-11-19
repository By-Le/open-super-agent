//! 电脑工具（待迁：osa/computer/tools.py，第 2 步）。
use crate::task::Task;
use serde_json::{Value, json};
use std::sync::Arc;

tokio::task_local! {
    pub static CLAIM: String;
}

pub async fn release() {}

fn names() -> Vec<String> {
    let s = crate::assets::schemas();
    s["computer_schemas"]
        .as_array()
        .into_iter()
        .chain(s["desk_schemas"].as_array())
        .flatten()
        .filter_map(|x| x["function"]["name"].as_str().map(String::from))
        .collect()
}

pub fn is_name(name: &str) -> bool {
    names().iter().any(|n| n == name)
}

pub fn is_desktop(name: &str) -> bool {
    crate::assets::schemas()["desk_schemas"].as_array().is_some_and(|a| a.iter().any(|x| x["function"]["name"] == name))
}

pub async fn call(
    _task: &Arc<Task>,
    _name: &str,
    _args: &Value,
    _progress: &(dyn Fn(String) + Sync),
    _key: Option<&str>,
) -> (String, Option<Vec<u8>>) {
    ("电脑现在用不了（Rust 版还没有电脑）。".into(), None)
}

pub fn desk_schemas() -> Vec<Value> {
    vec![]
}

pub fn schemas() -> Vec<Value> {
    crate::assets::schemas()["computer_schemas"].as_array().cloned().unwrap_or_default()
}

const BODY: &str = include_str!("../prompts/computer_body.txt");
const INTRO: &str = "你有一台电脑（Linux + Chromium 浏览器 + 终端，和其他 Bot 共用）。你的任务有自己的浏览器窗口，\n别的任务同时在别的窗口里做、互不干扰；登录状态和文件是大家共用的。用 browser_* / computer_* 工具操作：";

pub fn system_hint() -> String {
    format!("{INTRO}{}", BODY.replace("{shell}", "\n- 下载文件、处理数据、跑脚本用 computer_shell。"))
}

pub fn capability() -> String {
    "每个后台任务都能用所有 Bot 共用的一台电脑（Linux 桌面 + Chromium 浏览器 + 终端），可以打开网页、点击、输入、登录后操作、上传下载文件、运行脚本。几个任务可以同时用，各自一个浏览器窗口，登录状态和文件共用；同时用的任务多了才排队。用户能在右栏实时看到屏幕。".into()
}

pub fn prune_images(msgs: &mut [Value], keep: usize) {
    let mut seen = 0;
    for m in msgs.iter_mut().rev() {
        if m["content"].as_array().is_some_and(|a| a.iter().any(|p| p["type"] == "image_url")) {
            seen += 1;
            if seen > keep {
                m["content"] = json!("（更早的截图已省略）");
            }
        }
    }
}

pub fn image_message(png: &[u8], caption: Option<&str>, coords: bool) -> Value {
    let caption = caption.unwrap_or("当前页面截图");
    let w = png.get(16..20).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
    let h = png.get(20..24).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
    let tail = if coords { "点击坐标按这个尺寸" } else { "只用来看，不能按这张图的坐标点击" };
    json!({"role": "user", "content": [
        {"type": "text", "text": format!("（系统附图：{caption}，{w}×{h}，{tail}）")},
        {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", crate::util::b64(png))}}]})
}
