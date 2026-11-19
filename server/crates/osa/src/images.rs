//! 对话里画图：Router 的 generate_image 工具。对应 osa/images.py（流程见那里的说明）。
//! 用哪个模型画（settings 表的 image_model / image_edit_model）也在这里，Python 版写在 avatars.py：头像那边调这里的
//! models()、current()、choose()、draw()。

use crate::generation::{self, Ref};
use crate::registry::{Call, Out, PostFn};
use crate::task::{NewTask, Task};
use crate::{approvals, config, db, library, media, memory, py};
use regex::Regex;
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const TOOL: &str = "generate_image";
pub const MAX_REFS: usize = 16;

pub const NO_MODEL: &str = "没画：还没有能画图的模型。照实告诉用户：去 设置 → 模型 添加有画图模型的服务（硅基流动的 Kolors 免费，还有 OpenAI 的 gpt-image、智谱的 CogView 等），再在“画图”那一行选模型。不要假装画了，也不要派任务去画。";
pub const NO_REF_MODEL: &str = "没画：现在的画图模型都不能照着图片画。照实告诉用户：去 设置 → 模型 添加能照着图片画的服务（硅基流动的 Qwen-Image-Edit、Kolors，OpenAI 的 gpt-image，火山方舟的 Seedream 4 以后的版本），再在“画图”里选“照着图片画”用的模型。也可以不带参考图、只按描述画。不要假装画了。";

// ---------- 用哪个模型画（Python 版 avatars.py 的后半） ----------
fn setting(key: &str) -> Value {
    memory::setting(key).filter(|v| v.is_object()).unwrap_or(json!({}))
}

fn choice_raw() -> Value {
    let s = setting("image_model");
    if s.as_object().is_some_and(|o| !o.is_empty()) { s } else { setting("avatar_model") }
}

/// 记下画图用哪个模型。edit：照着图片画用的那个
pub fn choose(provider: &str, model: &str, edit: bool) {
    memory::set_setting(if edit { "image_edit_model" } else { "image_model" }, &json!({"provider": provider, "model": model}));
}

/// 设置里选的画图模型：{provider, model}，没选过是空的
pub fn choice(edit: bool) -> Value {
    let c = if edit { setting("image_edit_model") } else { choice_raw() };
    json!({"provider": c["provider"].as_str().unwrap_or(""), "model": c["model"].as_str().unwrap_or("")})
}

fn sort_key(kind: &str) -> impl Fn(&Value) -> (usize, String, String) + '_ {
    move |x: &Value| (generation::rank(kind, x["model"].as_str().unwrap_or("")), py::str_of(&x["provider"]), py::str_of(&x["model"]))
}

fn pick_of(items: &[Value], c: &Value) -> Value {
    items.iter().find(|x| x["provider"] == c["provider"] && x["model"] == c["model"]).cloned().or_else(|| items.first().cloned()).unwrap_or(Value::Null)
}

/// 能画图的模型：models、default（直接画的）；edit_models、edit_default（能照着图片画的）
pub async fn models() -> Value {
    let out = generation::catalog("image").await;
    let mut draw: Vec<Value> = out.iter().filter(|x| x["needs_ref"] != true).cloned().collect();
    draw.sort_by_key(sort_key("image"));
    let mut edit: Vec<Value> = out.iter().filter(|x| py::truthy(&x["refs"])).cloned().collect();
    edit.sort_by_key(sort_key("edit"));
    let d = pick_of(&draw, &choice_raw());
    let e = pick_of(&edit, &choice(true));
    json!({"models": draw, "default": d, "edit_models": edit, "edit_default": e})
}

/// 现在画图用的模型；refs：带了几张参考图
pub async fn current(refs: usize) -> Option<Value> {
    let d = models().await;
    if refs == 0 {
        return Some(d["default"].clone()).filter(|v| !v.is_null());
    }
    let ok: Vec<Value> = d["edit_models"].as_array().cloned().unwrap_or_default().into_iter().filter(|x| x["refs"].as_i64().unwrap_or(0) >= refs as i64).collect();
    let c = choice(true);
    ok.iter().find(|x| x["provider"] == c["provider"] && x["model"] == c["model"]).cloned().or_else(|| ok.first().cloned())
}

/// 画一张（直接画的用了哪个模型记下来，下次默认用它）
pub async fn draw(prompt: &str, provider: &str, model: &str, refs: &[Ref]) -> Result<(Vec<u8>, String), String> {
    let r = generation::draw(prompt, provider, model, refs).await?;
    if refs.is_empty() {
        choose(provider, model, false);
    }
    Ok(r)
}

pub fn price_text(price: Option<f64>) -> String {
    match price {
        None => t!("价格未知"),
        Some(p) if p == 0.0 => t!("免费"),
        Some(p) => t!("{price} / 张", price = format!("{}{}", config::get_str("pricing.currency"), crate::websearch::fmt_g(p))),
    }
}

fn name_of(prompt: &str) -> String {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\\/:*?"<>|\s]+"#).unwrap());
    let w = R.replace_all(prompt.trim(), "-").into_owned();
    let w = py::head(&w, 24).trim_matches('-').to_string();
    format!("{}.png", if w.is_empty() { "image".into() } else { w })
}

/// 消息附件里存过的这张图（按路径）
pub fn by_path(path: &str, video: bool) -> Option<Value> {
    let mut cands = vec![path.to_string()];
    if let Ok(n) = library::norm(path, None) {
        if !cands.contains(&n) {
            cands.push(n);
        }
    }
    for p in cands {
        let r = db::row(
            "select json_extract(a.value, '$.media') from messages m, json_each(m.meta, '$.attachments') a
            where m.meta like '%\"media\"%' and json_extract(a.value, '$.path') = ? and json_extract(a.value, '$.media') is not null
            order by m.id desc limit 1",
            params![p],
        );
        if let Some(got) = r.and_then(|r| media::info(&r[0])) {
            if !video || got["mime"].as_str().unwrap_or("").starts_with("video/") {
                return Some(got);
            }
        }
    }
    None
}

static MID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{24}$").unwrap());

fn list_of(items: &Value, max: usize) -> Vec<String> {
    let v: Vec<Value> = match items {
        Value::Array(a) => a.clone(),
        x if py::truthy(x) => vec![x.clone()],
        _ => vec![],
    };
    v.into_iter().take(max).map(|x| if py::truthy(&x) { py::str_of(&x) } else { String::new() }).collect()
}

fn strip_mark(x: &str, mark: &str) -> String {
    let x = x.trim();
    let x = x.strip_prefix(mark).unwrap_or(x);
    let x = x.strip_prefix("[附件]").unwrap_or(x);
    x.trim().to_string()
}

/// 参考图 → media 里的图片信息。写的是编号或路径，找不到、不是图片时报错
pub async fn find_refs(items: &Value) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = vec![];
    for x in list_of(items, MAX_REFS) {
        let x = strip_mark(&x, "[图片]");
        if x.is_empty() {
            continue;
        }
        let got = if MID.is_match(&x) {
            media::info(&json!(x)).ok_or_else(|| format!("对话里没有编号是 {x} 的图片"))?
        } else {
            match by_path(&x, false) {
                Some(g) => g,
                // 电脑里的文件要等电脑迁过来（第 2 步）
                None => return Err(format!("电脑里没有这个文件：{x}")),
            }
        };
        let mime = got["mime"].as_str().unwrap_or("");
        if !media::MODEL_MIMES.contains(&mime) {
            return Err(format!("{} 是 {mime}，画图模型只收 PNG、JPG、WebP", py::str_of(&got["name"])));
        }
        if out.iter().all(|r| r["id"] != got["id"]) {
            out.push(got);
        }
    }
    Ok(out)
}

fn load(refs: &[Value]) -> Result<Vec<Ref>, String> {
    refs.iter()
        .map(|r| {
            let (mime, data, _) = media::get(r["id"].as_str().unwrap_or("")).ok_or_else(|| t!("参考图已经被清理掉了：{name}", name = py::str_of(&r["name"])))?;
            Ok(Ref { data, mime, name: py::str_of(&r["name"]), role: "image".into(), w: r["w"].as_i64().unwrap_or(0), h: r["h"].as_i64().unwrap_or(0), ..Default::default() })
        })
        .collect()
}

async fn make(chat_id: i64, prompt: &str, m: &Value, post: &PostFn, refs: &[Value]) -> Result<String, String> {
    let (provider, model) = (py::str_of(&m["provider"]), py::str_of(&m["model"]));
    let (data, _) = draw(prompt, &provider, &model, &load(refs)?).await?;
    let item = media::save(&data, &name_of(prompt), generation::MAX_IMAGE, false)?;
    let price = generation::image_price(&provider, &model);
    let att = json!({"name": item["name"], "size": item["size"], "media": item["id"], "mime": item["mime"], "w": item["w"], "h": item["h"]});
    let mut meta = json!({"kind": "image", "prompt": prompt, "model": model, "provider": provider, "price": price, "attachments": [att]});
    if !refs.is_empty() {
        meta["refs"] = json!(refs.iter().map(|r| json!({"media": r["id"], "name": r["name"]})).collect::<Vec<_>>());
    }
    post("task".into(), format!("[画图] {prompt}"), chat_id, meta).await;
    Ok("已画好，图片已经显示在对话里，用户看得到。不用描述画面，简短说一句就行。".into())
}

/// generate_image 工具
pub async fn generate(bot: &Value, chat_id: i64, private_chat_id: i64, prompt: &str, post: PostFn, images: &Value) -> String {
    let prompt = py::head(prompt.trim(), 1000);
    if prompt.is_empty() {
        return "没画：描述是空的".into();
    }
    let refs = match find_refs(images).await {
        Ok(r) => r,
        Err(e) => return format!("没画：参考图有问题：{e}"),
    };
    let Some(m) = current(refs.len()).await else {
        if refs.is_empty() {
            return NO_MODEL.into();
        }
        let most = models().await["edit_models"].as_array().map(|a| a.iter().filter_map(|x| x["refs"].as_i64()).max().unwrap_or(0)).unwrap_or(0);
        if most == 0 {
            return NO_REF_MODEL.into();
        }
        return format!("没画：能用的画图模型最多参考 {most} 张图，你给了 {} 张。挑最要紧的 {most} 张再画，或者问用户用哪几张。", refs.len());
    };
    let price = generation::image_price(&py::str_of(&m["provider"]), &py::str_of(&m["model"]));
    if !approvals::decide(TOOL, None, &Value::Null).0 {
        return match make(chat_id, &prompt, &m, &post, &refs).await {
            Ok(s) => s,
            Err(e) => format!("没画成：{e}"),
        };
    }
    let name = py::str_of(&bot["name"]);
    let fake = Task::new(NewTask {
        fake_id: Some(format!("对话·{name}")),
        goal: t!("在对话里画图"),
        bot_id: bot["id"].as_i64().unwrap_or(0),
        chat_id,
        approval_chat_id: private_chat_id,
        ..Default::default()
    });
    let mut shown = json!({"描述": prompt});
    if !refs.is_empty() {
        shown["参考图"] = json!(refs.iter().map(|r| py::str_of(&r["name"])).collect::<Vec<_>>().join("、"));
    }
    shown["模型"] = json!(format!("{} · {}", py::str_of(&m["model"]), py::str_of(&m["title"])));
    shown["价格"] = json!(price_text(price));
    let (m2, p2) = (m.clone(), prompt.clone());
    tokio::spawn(async move {
        let (ok, _, _) = approvals::request(&fake, TOOL, &shown, approvals::Req::default()).await;
        if !ok {
            return;
        }
        if let Err(e) = make(chat_id, &p2, &m2, &post, &refs).await {
            post("task".into(), t!("[没画成：{error}]", error = e), chat_id, json!({"kind": "note"})).await;
        }
    });
    format!("已弹出确认卡片（模型 {}，{}），用户同意后会自动画好、显示在对话里，你不用再回来。简短告诉用户确认一下就行，不要说已经画好。", py::str_of(&m["model"]), price_text(price))
}

pub async fn tool_generate_image(c: Call) -> Out {
    let Some(post) = c.post.clone() else { return Out::Text("没画：这里不能画图".into()) };
    Out::Text(generate(&c.bot, c.chat_id, c.private_chat_id, &crate::builtin::arg_s(&c.args, "prompt"), post, &c.args["images"]).await)
}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
