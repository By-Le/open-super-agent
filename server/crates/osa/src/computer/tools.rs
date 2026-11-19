//! Worker 的电脑工具。对应 osa/computer/tools.py（说明见那里）。schema 取自 Python 版导出的 assets（逐字一样）。

use super::base::{Computer, ComputerError, HOSTS, SHELL_MAX_S};
use super::current;
use crate::task::Task;
use crate::{approvals, assets, i18n, py, watchdog, websearch};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

tokio::task_local! {
    /// 当前 Worker 占用电脑用的标识（Heavy 模式下同一任务的几个 Worker 各不相同）
    pub static CLAIM: String;
}

/// 一张截图，带说明（整个屏幕的截图和浏览器页面的截图，给模型的说明不一样）
#[derive(Clone, Debug)]
pub struct Shot {
    pub png: Vec<u8>,
    pub caption: Option<String>,
}

impl Shot {
    pub fn page(png: Vec<u8>) -> Shot {
        Shot { png, caption: None }
    }
}

const AUTO_SHOT: [&str; 4] = ["computer_click", "computer_type", "computer_key", "computer_scroll"];
const BROWSER: [&str; 9] = ["browser_open", "browser_snapshot", "browser_click", "browser_type", "browser_key", "browser_scroll", "browser_back", "browser_read", "browser_upload"];

fn is_window(name: &str) -> bool {
    BROWSER.contains(&name) || ["computer_screenshot", "computer_click", "computer_type", "computer_key", "computer_scroll", "ask_human"].contains(&name)
}

fn schema_list(k: &str) -> Vec<Value> {
    assets::schemas()[k].as_array().cloned().unwrap_or_default()
}

fn names_in(k: &str) -> Vec<String> {
    schema_list(k).iter().filter_map(|x| x["function"]["name"].as_str().map(String::from)).collect()
}

static NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut v = names_in("computer_schemas");
    v.extend(names_in("desk_schemas"));
    v
});
static DESKTOP: LazyLock<Vec<String>> = LazyLock::new(|| names_in("desk_schemas"));

pub fn is_name(name: &str) -> bool {
    NAMES.iter().any(|n| n == name)
}

pub fn is_desktop(name: &str) -> bool {
    DESKTOP.iter().any(|n| n == name)
}

pub async fn release() {
    if let Ok(key) = CLAIM.try_with(|k| k.clone()) {
        if !key.is_empty() {
            current().release(&key).await;
        }
    }
}

/// 把浏览器返回的页面信息排成给模型看的文字
pub fn page_text(d: &Value) -> String {
    let s = |k: &str| -> String { if d[k].is_null() { String::new() } else { py::str_of(&d[k]) } };
    if d.get("elements").is_none() {
        let off = py::to_int(&d["offset"]).unwrap_or(0);
        let text = s("text");
        let total = py::to_int(&d["total"]).unwrap_or(0);
        let n = py::char_len(&text) as i64;
        let more = if off + n < total { format!("\n…（共 {total} 字，用 offset={} 继续读）", off + n) } else { String::new() };
        return format!("{} | {}\n{text}{more}", s("title"), s("url"));
    }
    let els: Vec<String> = d["elements"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default();
    let mut els = if els.is_empty() { "（没有可操作元素）".to_string() } else { els.join("\n") };
    if py::truthy(&d["dialog"]) {
        els = format!("（页面上有弹窗，下面只列弹窗里的元素；是登录、扫码或验证码就调用 ask_human）\n{els}");
    }
    format!("当前页面：{} | {}（滚动 {}）\n可操作元素：\n{els}\n\n正文开头：{}", s("title"), s("url"), s("scroll"), py::head(&s("text"), 1200))
}

fn fun(name: &str, desc: &str, props: Value, required: Value) -> Value {
    json!({"type": "function", "function": {"name": name, "description": desc, "parameters": {"type": "object", "properties": props, "required": required}}})
}

const HOST_FILES: [(&str, &str); 3] = [
    ("computer_read_file", "读取工作文件夹里的文本文件（路径可以写相对工作文件夹的）"),
    ("computer_write_file", "在工作文件夹里写入文本文件（会覆盖；路径可以写相对工作文件夹的）"),
    ("browser_upload", "把工作文件夹里的文件传到网页上：ref 是“上传 / 选择文件”按钮或文件输入框的编号，paths 是文件路径"),
];

/// 操作整个屏幕的工具：只有本机（或借来的 Mac）是 Mac、给了两个权限才给
pub fn desk_schemas_for(c: &Computer) -> Vec<Value> {
    if HOSTS.contains(&c.kind()) && c.backend.desktop_enabled() { schema_list("desk_schemas") } else { vec![] }
}

pub fn desk_schemas() -> Vec<Value> {
    desk_schemas_for(&current())
}

/// 给 Worker 的电脑工具
pub fn schemas() -> Vec<Value> {
    let c = current();
    if !HOSTS.contains(&c.kind()) {
        return schema_list("computer_schemas");
    }
    let mut out = vec![];
    for s in schema_list("computer_schemas") {
        let f = &s["function"];
        let name = f["name"].as_str().unwrap_or("");
        if name == "computer_shell" {
            if !c.backend.shell_enabled() {
                continue;
            }
            out.push(fun(
                "computer_shell",
                &format!("在用户这台电脑上执行 shell 命令（工作目录是工作文件夹 {}）。每条命令都要用户确认。", c.workspace()),
                json!({"cmd": {"type": "string"}, "timeout": {"type": "integer"}}),
                json!(["cmd"]),
            ));
        } else if let Some((_, d)) = HOST_FILES.iter().find(|(n, _)| *n == name) {
            let mut s2 = s.clone();
            s2["function"]["description"] = json!(d);
            out.push(s2);
        } else {
            out.push(s);
        }
    }
    out.extend(desk_schemas_for(&c));
    out
}

const BODY: &str = include_str!("../prompts/computer_body.txt");
const DESK_HINT: &str = include_str!("../prompts/computer/desk_hint.txt");

fn intro(c: &Computer) -> String {
    if !HOSTS.contains(&c.kind()) {
        return "你有一台电脑（Linux + Chromium 浏览器 + 终端，和其他 Bot 共用）。你的任务有自己的浏览器窗口，\n别的任务同时在别的窗口里做、互不干扰；登录状态和文件是大家共用的。用 browser_* / computer_* 工具操作：".into();
    }
    let shell = if c.backend.shell_enabled() {
        "- 终端：computer_shell 在用户这台电脑上执行命令（工作目录是工作文件夹）。每条命令都要用户确认（用户“总是允许”过的命令开头除外，带 ; && | $( 重定向的命令每次都问），只在必要时用。"
    } else {
        "- 没有终端（用户没有打开）。下载文件用浏览器，读写文件用 computer_read_file / computer_write_file。"
    };
    let desk = if c.backend.desktop_enabled() { DESK_HINT } else { "" };
    let where_ = if c.kind() == "mac" {
        format!("这个任务派到了用户的 Mac「{}」上做（任务要用的东西只有那台 Mac 上有）。你用的电脑就是那台 Mac：", c.backend.name())
    } else {
        "你有一台电脑：".into()
    };
    format!(
        "{where_}用户这台电脑上专门给 Bot 开的 Chrome 浏览器（和用户自己日常用的 Chrome 分开，所有 Bot 共用，\n用户在里面登录过的网站你都能直接用）。你的任务有自己的浏览器窗口，别的任务同时在别的窗口里做、互不干扰。用 browser_* / computer_* 工具操作：\n- 文件只能放在工作文件夹 {} 里：computer_read_file / computer_write_file 只能读写这里面的文件（可以写相对路径），\n  浏览器下载的文件在它的 downloads 子文件夹，browser_upload 也只能传这里面的文件。\n{shell}\n- 上传文件、写文件、点“发送 / 发布 / 提交 / 付款”这类按钮时，系统会自动弹确认卡片问用户，这些不用再调用 ask_approval；\n  用户拒绝了就别再试，换个做法或者在结果里说明。{desk}",
        c.workspace()
    )
}

pub fn system_hint() -> String {
    let c = current();
    let shell = if HOSTS.contains(&c.kind()) { "" } else { "\n- 下载文件、处理数据、跑脚本用 computer_shell。" };
    intro(&c) + &BODY.replace("{shell}", shell)
}

/// Router 的能力清单里那一句：电脑是什么、能做什么
pub fn capability() -> String {
    let c = current();
    if HOSTS.contains(&c.kind()) {
        return format!(
            "每个后台任务都能用用户这台电脑上专门给 Bot 开的 Chrome 浏览器（和用户自己的 Chrome 分开，所有 Bot 共用），可以打开网页、点击、输入、登录后操作、上传下载文件（文件放在工作文件夹）{}。几个任务可以同时用，各自一个浏览器窗口，登录状态共用；同时用的任务多了才排队。浏览器窗口就在用户的屏幕上，右栏也能看到画面。上传文件、写文件、点“发送 / 发布 / 付款”这类按钮时会自动弹确认卡片。{}",
            if c.backend.shell_enabled() { "、在终端执行命令（每条都要用户确认）" } else { "" },
            if c.backend.desktop_enabled() {
                "任务还能看整个屏幕、用真实的鼠标键盘操作这台 Mac 上的其他 App（微信、访达、备忘录、系统设置等，desktop_* 工具），每个任务第一次用会先问用户；鼠标键盘一次只能一个任务用。你自己用 desktop_* 只做一两步的事（打开某个 App、看一眼屏幕）；要在 App 里按一串按钮、填东西、来回操作的，派一个任务去做（你这一轮能调用的工具次数有限）。"
            } else {
                ""
            }
        );
    }
    "每个后台任务都能用所有 Bot 共用的一台电脑（Linux 桌面 + Chromium 浏览器 + 终端），可以打开网页、点击、输入、登录后操作、上传下载文件、运行脚本。几个任务可以同时用，各自一个浏览器窗口，登录状态和文件共用；同时用的任务多了才排队。用户能在右栏实时看到屏幕。".into()
}

fn label_of(task: &Task, key: &str) -> String {
    if let Some(f) = &task.fake_id {
        return f.clone();
    }
    let part = key.rsplit(':').next().unwrap_or("");
    let mut out = t!("任务 #{id}", id = task.id);
    if let Some(n) = part.strip_prefix('h').filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
        out += &format!(" · {}", t!("研究员 {n}", n = n.parse::<i64>().unwrap_or(0) + 1));
    }
    out
}

fn a(args: &Value, k: &str) -> Value {
    args.get(k).cloned().unwrap_or(Value::Null)
}

fn as_s(v: &Value) -> String {
    if v.is_null() { String::new() } else { py::str_of(v) }
}

/// 任务卡片和任务过程里显示的这一步在做什么
fn progress_label(name: &str, args: &Value) -> String {
    match name {
        "browser_snapshot" => t!("查看页面"),
        "browser_back" => t!("后退"),
        "browser_read" => t!("读取页面"),
        "computer_screenshot" => t!("截图"),
        "browser_open" => t!("打开 {url}", url = as_s(&args.get("url").cloned().unwrap_or(json!("")))),
        "browser_click" => crate::i18n::t("点击 [{ref}]", &[("ref", a(args, "ref"))]),
        "browser_type" | "computer_type" => t!("输入 “{text}”", text = py::head(&as_s(&args.get("text").cloned().unwrap_or(json!(""))), 30)),
        "browser_key" => t!("按键 {key}", key = a(args, "key")),
        "computer_key" => t!("按键 {key}", key = a(args, "keys")),
        "browser_scroll" | "computer_scroll" => t!("滚动 {direction}", direction = a(args, "direction")),
        "browser_upload" => crate::i18n::t("上传文件到 [{ref}]", &[("ref", a(args, "ref"))]),
        "computer_click" => t!("点击 ({x}, {y})", x = a(args, "x"), y = a(args, "y")),
        "computer_shell" => t!("终端：{cmd}", cmd = py::head(&as_s(&args.get("cmd").cloned().unwrap_or(json!(""))), 60)),
        "computer_read_file" => t!("读取 {path}", path = a(args, "path")),
        "computer_write_file" => t!("写入 {path}", path = a(args, "path")),
        "desktop_screenshot" => t!("看屏幕"),
        "desktop_click" => t!("点击屏幕 ({x}, {y})", x = a(args, "x"), y = a(args, "y")),
        "desktop_drag" => t!("在屏幕上拖动"),
        "desktop_type" => t!("输入 “{text}”", text = py::head(&as_s(&args.get("text").cloned().unwrap_or(json!(""))), 30)),
        "desktop_key" => t!("按键 {key}", key = a(args, "keys")),
        "desktop_scroll" => t!("滚动 {direction}", direction = a(args, "direction")),
        "desktop_open_app" => t!("打开 {app}", app = as_s(&args.get("name").cloned().unwrap_or(json!("")))),
        "ask_human" => t!("请你帮忙：{reason}", reason = py::head(&as_s(&args.get("reason").cloned().unwrap_or(json!(""))), 60)),
        "ask_approval" => t!("请求确认：{action}", action = py::head(&as_s(&args.get("action").cloned().unwrap_or(json!(""))), 60)),
        _ => name.to_string(),
    }
}

fn is_enter(s: &str) -> bool {
    s == "enter" || s == "return"
}

/// 按确认规则先问用户。返回 None 表示可以做，否则是给模型的话
async fn confirm(task: &Arc<Task>, c: &Computer, name: &str, args: &Value, key: &str) -> Option<String> {
    let (tool, shown) = match name {
        "browser_upload" => (name.to_string(), json!({"要上传的文件": a(args, "paths"), "上传到": format!("[{}]", py::str_of(&a(args, "ref")))})),
        "computer_write_file" => (name.to_string(), json!({"文件": a(args, "path"), "内容开头": py::head(&as_s(&a(args, "content")), 500)})),
        "computer_shell" => (name.to_string(), json!({"命令": a(args, "cmd")})),
        _ => {
            let mut mode = if name == "browser_click" || name == "computer_click" { Some("click") } else { None };
            let key_s = as_s(&a(args, "key")).to_lowercase();
            let keys_s = as_s(&a(args, "keys")).replace(' ', "").to_lowercase();
            if (name == "browser_type" && py::truthy(&a(args, "submit")))
                || (name == "browser_key" && is_enter(&key_s))
                || (name == "computer_key" && is_enter(keys_s.rsplit('+').next().unwrap_or("")))
            {
                mode = Some("enter");
            }
            let mode = mode?;
            if !approvals::decide("browser_submit", Some(c.kind()), &Value::Null).0 {
                return None;
            }
            let info = c
                .browser("inspect", Some(key), false, json!({"mode": mode, "ref": a(args, "ref"), "x": a(args, "x"), "y": a(args, "y")}))
                .await
                .ok()?;
            if !py::truthy(&info["submit"]) {
                return None;
            }
            ("browser_submit".to_string(), json!({"操作": info["what"], "页面": format!("{} | {}", as_s(&info["title"]), as_s(&info["url"]))}))
        }
    };
    let check = if tool != "browser_submit" { args.clone() } else { shown.clone() };
    if !approvals::decide(&tool, Some(c.kind()), &check).0 {
        return None;
    }
    let (ok, note, _) = approvals::request(task, &tool, &shown, approvals::Req::default()).await;
    if ok { None } else { Some(format!("用户拒绝了这个操作，没有执行。{note}").trim().to_string()) }
}

/// 执行一个电脑工具，返回 (给模型的文字, 可选截图)。key 是谁在用：任务的 CLAIM，对话里直接操作的是 chat-<Bot id>
pub async fn call(task: &Arc<Task>, name: &str, args: &Value, progress: &(dyn Fn(String) + Sync), key: Option<&str>) -> (String, Option<Shot>) {
    let c = current();
    let key = key
        .map(String::from)
        .filter(|k| !k.is_empty())
        .or_else(|| CLAIM.try_with(|k| k.clone()).ok().filter(|k| !k.is_empty()))
        .unwrap_or_else(|| py::str_of(&task.id_value()));
    if is_desktop(name) {
        progress(progress_label(name, args));
        return match desktop(task, &c, name, args, &key, progress).await {
            Ok(r) => r,
            Err(DeskErr::Pc(e)) => (format!("电脑操作失败：{e}"), None),
            Err(DeskErr::Arg(e)) => (format!("参数不对：{e}"), None),
        };
    }
    if is_window(name) && !key.starts_with("chat-") {
        let tid = if task.fake_id.is_some() { Value::Null } else { json!(task.id) };
        c.claim(&key, &label_of(task, &key), Some(progress), tid, json!(task.bot_id)).await;
    }
    progress(progress_label(name, args));
    match run(task, &c, name, args, &key).await {
        Ok(r) => r,
        Err(e) => (format!("电脑操作失败：{e}"), None),
    }
}

async fn run(task: &Arc<Task>, c: &Arc<Computer>, name: &str, args: &Value, key: &str) -> Result<(String, Option<Shot>), ComputerError> {
    let mut args = args.clone();
    if name == "browser_upload" && args["paths"].is_string() {
        args["paths"] = json!([args["paths"].clone()]);
    }
    let mut paths: Vec<String> = vec![];
    if name == "browser_upload" {
        let given: Vec<String> = args["paths"].as_array().map(|p| p.iter().take(10).map(py::str_of).collect()).unwrap_or_default();
        paths = c.backend.upload_paths(given)?;
        if paths.is_empty() {
            return Ok(("没有给要上传的文件路径".into(), None));
        }
    }
    if let Some(refused) = confirm(task, c, name, &args, key).await {
        return Ok((refused, None));
    }
    if name == "browser_upload" {
        c.browser("upload", Some(key), false, json!({"ref": a(&args, "ref"), "files": paths})).await?;
        return Ok((page_text(&c.browser("snapshot", Some(key), false, json!({})).await?), None));
    }
    if BROWSER.contains(&name) {
        let op = name.strip_prefix("browser_").unwrap_or(name);
        if op == "open" {
            let url = as_s(&a(&args, "url"));
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Ok(("只能打开 http / https 网址".into(), None));
            }
            if websearch::blocked_domain(&url).is_some() {
                return Ok((websearch::refuse_text(&url, false), None));
            }
        }
        let mut kw = serde_json::Map::new();
        if let Some(o) = args.as_object() {
            for (k, v) in o {
                if v.is_null() {
                    continue;
                }
                kw.insert(if k == "key" { "name".into() } else { k.clone() }, v.clone());
            }
        }
        return Ok((page_text(&c.browser(op, Some(key), false, Value::Object(kw)).await?), None));
    }
    let text: String = match name {
        "computer_screenshot" => {
            let png = c.page_shot(key).await?;
            if !py::truthy(&a(&args, "save")) {
                return Ok(("这是浏览器里当前页面的截图。".into(), Some(Shot::page(png))));
            }
            let path = format!("screenshots/网页 {}.png", py::strftime("%Y-%m-%d %H.%M.%S"));
            let full = if HOSTS.contains(&c.kind()) { path.clone() } else { format!("{}/{path}", c.workspace()) };
            c.write_bytes(&full, &png).await?;
            return Ok((format!("这是浏览器里当前页面的截图，已存到 {}/{path}（发给用户用 send_file）。", c.workspace()), Some(Shot::page(png))));
        }
        "computer_click" => {
            let button = if py::truthy(&a(&args, "button")) { as_s(&a(&args, "button")) } else { "left".into() };
            if !["left", "right", "middle"].contains(&button.as_str()) {
                return Ok(("button 只能是 left / right / middle".into(), None));
            }
            let (x, y) = (py::to_int(&a(&args, "x")).ok_or_else(|| ComputerError("KeyError: 'x'".into()))?, py::to_int(&a(&args, "y")).ok_or_else(|| ComputerError("KeyError: 'y'".into()))?);
            c.browser("click_xy", Some(key), false, json!({"x": x, "y": y, "button": button, "double": py::truthy(&a(&args, "double"))})).await?;
            "已点击。".into()
        }
        "computer_type" => {
            if !py::truthy(&a(&args, "text")) {
                return Ok(("没有要输入的文字".into(), None));
            }
            c.browser("type", Some(key), false, json!({"text": as_s(&a(&args, "text"))})).await?;
            "已输入。".into()
        }
        "computer_key" => {
            c.browser("press", Some(key), false, json!({"keys": as_s(&a(&args, "keys"))})).await?;
            "已按键。".into()
        }
        "computer_scroll" => {
            let dir = if py::truthy(&a(&args, "direction")) { a(&args, "direction") } else { json!("down") };
            let amount = if py::truthy(&a(&args, "amount")) { a(&args, "amount") } else { json!(3) };
            c.browser("wheel", Some(key), false, json!({"direction": dir, "amount": amount})).await?;
            "已滚动。".into()
        }
        "computer_shell" => {
            let secs = py::to_int(&a(&args, "timeout")).filter(|t| *t != 0).unwrap_or(120).clamp(1, SHELL_MAX_S);
            let Some(cmd) = args["cmd"].as_str().map(String::from) else {
                return Err(ComputerError("KeyError: 'cmd'".into()));
            };
            let _g = watchdog::long_call(secs as f64);
            return Ok((c.shell(&cmd, secs).await?.text(8000), None));
        }
        "computer_read_file" => {
            let data = c.read_bytes(&as_s(&a(&args, "path")), 1 << 20).await?;
            return Ok((py::head(&String::from_utf8_lossy(&data), 20000), None));
        }
        "computer_write_file" => {
            let path = as_s(&a(&args, "path"));
            let content = as_s(&a(&args, "content"));
            c.write_bytes(&path, content.as_bytes()).await?;
            return Ok((format!("已写入 {path}（{} 字）", py::char_len(&content)), None));
        }
        "ask_human" => {
            c.start().await?;
            if c.controller() == "bot" && !HOSTS.contains(&c.kind()) {
                let _ = c.front(key).await;
            }
            let tid = py::str_of(&task.id_value());
            c.st.lock().asking.insert(tid.clone(), key.into());
            let (ok, note, _) = approvals::request(task, "ask_human", &json!({"需要你做的事": a(&args, "reason")}), approvals::Req { kind: "help", ..Default::default() }).await;
            {
                let mut s = c.st.lock();
                if s.asking.get(&tid).map(|k| k.as_str()) == Some(key) {
                    s.asking.remove(&tid);
                }
            }
            return Ok((if ok { format!("用户已处理完并交还电脑。{note}").trim().to_string() } else { format!("用户没有处理：{note}").trim().to_string() }, None));
        }
        "ask_approval" => {
            let (ok, note, _) = approvals::request(task, "ask_approval", &json!({"操作": a(&args, "action")}), approvals::Req { once: true, ..Default::default() }).await;
            return Ok((if ok { format!("用户同意了。{note}").trim().to_string() } else { format!("用户拒绝了，不要执行这个操作。{note}").trim().to_string() }, None));
        }
        _ => return Ok(("未知的电脑工具".into(), None)),
    };
    let shot = if AUTO_SHOT.contains(&name) { Some(Shot::page(c.page_shot(key).await?)) } else { None };
    Ok((text, shot))
}

// ---------- 整个屏幕（desktop_*，本机 / 派到的 Mac，第 2 步 B 的后端实现 desk_*） ----------
enum DeskErr {
    Pc(ComputerError),
    Arg(String),
}

impl From<ComputerError> for DeskErr {
    fn from(e: ComputerError) -> Self {
        DeskErr::Pc(e)
    }
}

static DESK_SUBMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)发送|发布|发表|提交|支付|付款|购买|下单|结算|转账|\b(send|post|publish|submit|pay|buy|order|checkout|transfer|tweet)\b").unwrap()
});

fn desk_text(d: &Value) -> String {
    let f = &d["front"];
    let wins: Vec<String> = d["windows"]
        .as_array()
        .map(|a| a.iter().map(|w| format!("{}{}", as_s(&w["app"]), if py::truthy(&w["title"]) { format!(" — {}", as_s(&w["title"])) } else { String::new() })).collect())
        .unwrap_or_default();
    let wins = if wins.is_empty() { "无".to_string() } else { wins.join("；") };
    format!(
        "屏幕截图 {}×{}（点击坐标按这个尺寸），鼠标在 ({}, {})。前台：{}{}。屏幕上的窗口（从前到后）：{wins}",
        as_s(&d["width"]),
        as_s(&d["height"]),
        as_s(&d["cursor"][0]),
        as_s(&d["cursor"][1]),
        if py::truthy(&f["app"]) { as_s(&f["app"]) } else { "未知".into() },
        if py::truthy(&f["title"]) { format!(" — {}", as_s(&f["title"])) } else { String::new() }
    )
}

fn desk_png(d: &Value) -> Vec<u8> {
    d["png"].as_str().and_then(crate::util::b64_decode).unwrap_or_default()
}

fn whole(png: Vec<u8>) -> Shot {
    Shot { png, caption: Some("整个屏幕的截图".into()) }
}

fn int_arg(args: &Value, k: &str) -> Result<i64, DeskErr> {
    py::to_int(&a(args, k)).ok_or_else(|| DeskErr::Arg(format!("'{k}'")))
}

/// desktop_key 里每一段的主键（macdesk.parse_keys 的第二项）
fn main_key(k: &str) -> String {
    k.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).last().unwrap_or("").to_lowercase()
}

async fn desktop(task: &Arc<Task>, c: &Arc<Computer>, name: &str, args: &Value, key: &str, progress: &(dyn Fn(String) + Sync)) -> Result<(String, Option<Shot>), DeskErr> {
    let b = &c.backend;
    if !HOSTS.contains(&c.kind()) || !b.desktop_enabled() {
        return Ok(("这台电脑现在不能操作整个屏幕（不是 Mac，或者还没给 Open Super Agent“辅助功能”和“屏幕录制”权限），只能用浏览器（browser_*）。".into(), None));
    }
    let ok_key = if key.starts_with("chat-") { key.to_string() } else { format!("task:{}", py::str_of(&task.id_value())) };
    let no = "用户这次没同意你操作整个屏幕（在卡片上点了“不行”）。能用浏览器做的用 browser_*，做不了就在结果里说明，这个任务里别再试 desktop_*。";
    if b.desk_denied(&ok_key).is_some_and(|t| py::now() - t < 600.0) {
        return Ok((no.into(), None));
    }
    b.desk_claim(c, key, &label_of(task, key), progress).await?;
    if !b.desk_allowed(&ok_key) {
        let goal = json!({"任务": py::head(&task.goal, 300)});
        if approvals::decide("desktop", Some(c.kind()), &goal).0 {
            let (ok, note, _) = approvals::request(task, "desktop", &goal, approvals::Req::default()).await;
            if !ok {
                b.set_desk_denied(&ok_key);
                b.desk_release(c, key).await;
                return Ok((format!("{no}{note}"), None));
            }
        }
        b.desk_ok(&ok_key);
    }
    if name == "desktop_screenshot" {
        let d = b.desk(c, "screenshot", key, &json!({})).await?;
        let png = desk_png(&d);
        let mut saved = String::new();
        if py::truthy(&a(args, "save")) {
            let path = format!("screenshots/屏幕 {}.png", py::strftime("%Y-%m-%d %H.%M.%S"));
            c.write_bytes(&path, &png).await?;
            saved = format!("截图已存到工作文件夹：{}/{path}（发给用户用 send_file）。", c.workspace());
        }
        return Ok((saved + &desk_text(&d), Some(whole(png))));
    }
    let mut what = String::new();
    let (op, kw, done): (&str, Value, String) = match name {
        "desktop_click" => {
            let (x, y) = (int_arg(args, "x")?, int_arg(args, "y")?);
            let button = if py::truthy(&a(args, "button")) { as_s(&a(args, "button")) } else { "left".into() };
            if !["left", "right", "middle"].contains(&button.as_str()) {
                return Ok(("button 只能是 left / right / middle".into(), None));
            }
            let el = b.desk(c, "inspect", key, &json!({"x": x, "y": y})).await?;
            let label = as_s(&el["label"]);
            if button == "left" && py::char_len(&label) <= 40 && DESK_SUBMIT.is_match(&label) {
                what = format!("在「{}」里点击「{label}」", if py::truthy(&el["app"]) { as_s(&el["app"]) } else { "?".into() });
            }
            let done = format!("已点击 ({x}, {y}){}。", if label.is_empty() { String::new() } else { format!("「{label}」") });
            ("click", json!({"x": x, "y": y, "button": button, "double": py::truthy(&a(args, "double"))}), done)
        }
        "desktop_drag" => ("drag", json!({"x": int_arg(args, "x")?, "y": int_arg(args, "y")?, "to_x": int_arg(args, "to_x")?, "to_y": int_arg(args, "to_y")?}), "已拖动。".into()),
        "desktop_type" => {
            if !py::truthy(&a(args, "text")) {
                return Ok(("没有要输入的文字".into(), None));
            }
            ("type", json!({"text": py::head(&as_s(&a(args, "text")), 20000)}), "已输入。".into())
        }
        "desktop_key" => {
            let keys = as_s(&a(args, "keys"));
            if keys.split_whitespace().any(|k| is_enter(&main_key(k))) {
                let f = b.desk(c, "focused", key, &json!({})).await?;
                if f["role"] == "AXTextArea" {
                    what = format!("在「{}」的输入框里按 {keys}", if py::truthy(&f["app"]) { as_s(&f["app"]) } else { "?".into() });
                }
            }
            ("key", json!({"keys": keys}), format!("已按 {keys}。"))
        }
        "desktop_scroll" => {
            let dir = if py::truthy(&a(args, "direction")) { a(args, "direction") } else { json!("down") };
            let amount = if py::truthy(&a(args, "amount")) { a(args, "amount") } else { json!(3) };
            ("scroll", json!({"x": int_arg(args, "x")?, "y": int_arg(args, "y")?, "direction": dir, "amount": amount}), "已滚动。".into())
        }
        "desktop_open_app" => {
            let app = as_s(&a(args, "name")).trim().to_string();
            if app.is_empty() {
                return Ok(("没有给 App 的名字".into(), None));
            }
            ("open_app", json!({"name": app}), format!("已打开 {app}。"))
        }
        _ => return Ok(("未知的电脑工具".into(), None)),
    };
    if !what.is_empty() && approvals::decide("desktop_submit", Some(c.kind()), &json!({"操作": what})).0 {
        let (ok, note, _) = approvals::request(task, "desktop_submit", &json!({"操作": what}), approvals::Req::default()).await;
        if !ok {
            return Ok((format!("用户拒绝了这个操作，没有执行。{note}").trim().to_string(), None));
        }
    }
    b.desk(c, op, key, &kw).await?;
    let settle = if name == "desktop_open_app" { 1.5 } else { 0.6 };
    tokio::time::sleep(std::time::Duration::from_secs_f64(settle)).await;
    let d = b.desk(c, "screenshot", key, &json!({})).await?;
    Ok((done + &desk_text(&d), Some(whole(desk_png(&d)))))
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

/// 只保留最近 keep 张截图，更早的换成文字
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

#[allow(dead_code)]
fn _i(_: i18n::Later) {}
