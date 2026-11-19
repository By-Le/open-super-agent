//! 应用市场和应用的接口：/api/market、/api/apps、/oauth、/api/vault、/api/rules、/api/mcp-apps。对应 osa/server.py 里那一段。

use crate::i18n::Later;
use crate::server::broadcast;
use crate::web::{ApiError, Body, Q, Res, ok, qs};
use crate::{approvals, bots, catalog, i18n, mcp_hub, mcp_oauth, mcpapps, py, security, vault};
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde_json::{Value, json};

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/apps", get(list_apps))
        .route("/api/market/{item_id}", post(install_app))
        .route("/api/apps/{name}", put(save_app).patch(patch_app).delete(delete_app))
        .route("/api/apps/{name}/{action}", post(app_action))
        .route("/oauth/start/{name}", get(oauth_start))
        .route("/oauth/callback", get(oauth_callback))
        .route("/api/rules", get(list_rules).post(add_rule))
        .route("/api/rules/{rid}", delete(delete_rule))
        .route("/api/vault", get(list_vault))
        .route("/api/vault/{name}", put(put_vault).delete(delete_vault))
        .route("/api/mcp-apps/{app}/view", get(mcp_app_view))
        .route("/api/mcp-apps/{app}/call", post(mcp_app_call))
        .route("/api/mcp-apps/{app}/read", post(mcp_app_read))
        .route("/api/mcp-apps/{app}/say", post(mcp_app_say))
}

async fn list_apps(h: HeaderMap) -> Res {
    let mut o = json!({});
    for (k, v) in crate::server::apps_out(&h) {
        o[k] = v;
    }
    ok(o)
}

/// 授权弹窗要回到的地址（和 /api/apps 给前端看的 oauth_callback 一样）
fn callback_url(h: &HeaderMap) -> String {
    crate::server::apps_out(h).into_iter().find(|(k, _)| *k == "oauth_callback").and_then(|(_, v)| v.as_str().map(String::from)).unwrap_or_default()
}

fn app_out(name: &str) -> Res {
    broadcast(json!({"type": "apps_changed"}));
    mcp_hub::public().into_iter().find(|x| x["name"] == name).map(axum::Json).ok_or_else(|| ApiError::not_found(t!("没有这个应用")))
}

async fn install_app(Path(item_id): Path<String>, Body(d): Body) -> Res {
    if mcp_hub::get(&item_id).is_some() {
        return Err(ApiError::bad(t!("已经添加过了")));
    }
    let values = if py::truthy(&d["values"]) { d["values"].clone() } else { json!({}) };
    security::check_market(&item_id)?;
    catalog::check(&item_id, &values).await?;
    let mut cfg = catalog::build(&item_id, &values)?;
    cfg["enabled"] = json!(true);
    mcp_hub::save(&item_id, &cfg)?;
    if catalog::items().get(&item_id).is_some_and(|it| it["kind"] == "oauth") {
        return app_out(&item_id);
    }
    let _ = mcp_hub::connect(&item_id, 150.0).await;
    if !mcp_hub::is_connected(&item_id) {
        let mut msg = mcp_hub::status_text(&item_id);
        if msg.is_empty() {
            msg = t!("没连上");
        }
        mcp_hub::disconnect(&item_id, false).await;
        mcp_hub::remove(&item_id);
        broadcast(json!({"type": "apps_changed"}));
        let words = ["401", "403", "400 Bad Request", "Unauthorized", "parse JSON", "密钥无效", "鉴权失败", "invalid or expired"];
        if words.iter().any(|w| msg.contains(w)) {
            msg = t!("对方没有接受这个密钥，检查一下有没有复制完整。（{msg}）", msg = py::head(&msg, 120));
        }
        return Err(ApiError::bad(msg));
    }
    app_out(&item_id)
}

fn ref_of(name: &str, k: &str) -> String {
    format!("{name}_{k}").to_uppercase().replace('-', "_")
}

async fn save_app(Path(name): Path<String>, Body(mut d): Body) -> Res {
    if security::demo() {
        return Err(ApiError::new(403, t!("演示服务器上只能添加应用市场里的应用，不能自定义或改高级设置")));
    }
    let old = mcp_hub::get(&name).unwrap_or(json!({}));
    let mut owned: Vec<String> = old["owned"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default();
    let mut env = if py::truthy(&d["env"]) { d["env"].clone() } else if py::truthy(&old["env"]) { old["env"].clone() } else { json!({}) };
    let secrets = d.as_object_mut().and_then(|o| o.remove("secrets")).unwrap_or(Value::Null);
    for (k, v) in secrets.as_object().cloned().unwrap_or_default() {
        if py::truthy(&v) {
            let r = ref_of(&name, &k);
            vault::put(&r, &py::str_of(&v))?;
            env[k.as_str()] = json!(format!("vault:{r}"));
            owned.push(r);
        }
    }
    let token = d.as_object_mut().and_then(|o| o.remove("token")).filter(|v| !v.is_null()).map(|v| py::str_of(&v)).unwrap_or_default();
    let token = token.trim().to_string();
    if !token.is_empty() {
        let r = ref_of(&name, "TOKEN");
        vault::put(&r, &token)?;
        let mut headers = if py::truthy(&d["headers"]) { d["headers"].clone() } else if py::truthy(&old["headers"]) { old["headers"].clone() } else { json!({}) };
        headers["Authorization"] = json!(format!("Bearer {{vault:{r}}}"));
        d["headers"] = headers;
        owned.push(r);
    }
    owned.sort();
    owned.dedup();
    d["env"] = env;
    d["owned"] = json!(owned);
    let a = mcp_hub::save(&name, &d)?;
    if a["enabled"] == true {
        let _ = mcp_hub::connect(&name, 150.0).await;
    } else {
        mcp_hub::disconnect(&name, false).await;
        mcp_hub::set_status(&name, Later::plain("未启用"));
    }
    app_out(&name)
}

async fn app_action(Path((name, action)): Path<(String, String)>) -> Res {
    if mcp_hub::get(&name).is_none() {
        return Err(ApiError::not_found(t!("没有这个应用")));
    }
    match action.as_str() {
        "enable" | "reconnect" => {
            mcp_hub::save(&name, &json!({"enabled": true}))?;
            let _ = mcp_hub::connect(&name, 150.0).await;
        }
        "disable" => {
            mcp_hub::save(&name, &json!({"enabled": false}))?;
            mcp_hub::disconnect(&name, false).await;
            mcp_hub::set_status(&name, Later::plain("未启用"));
        }
        "cancel" => mcp_oauth::cancel_login(&name),
        _ => return Err(ApiError::not_found(t!("未知操作"))),
    }
    app_out(&name)
}

async fn patch_app(Path(name): Path<String>, Body(d): Body) -> Res {
    if mcp_hub::get(&name).is_none() {
        return Err(ApiError::not_found(t!("没有这个应用")));
    }
    let mut upd = json!({});
    for k in ["deny", "confirm"] {
        if let Some(v) = d.get(k) {
            upd[k] = v.clone();
        }
    }
    let a = mcp_hub::save(&name, &upd)?;
    if a["enabled"] == true && mcp_hub::is_connected(&name) {
        let _ = mcp_hub::connect(&name, 150.0).await;
    }
    app_out(&name)
}

async fn delete_app(Path(name): Path<String>) -> Res {
    mcp_hub::disconnect(&name, false).await;
    mcp_hub::remove(&name);
    broadcast(json!({"type": "apps_changed"}));
    ok(json!({"ok": true}))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

/// 授权弹窗的最后一页：告诉用户结果，通知原页面，然后自己关掉
fn oauth_page(okay: bool, title: &str, detail: &str) -> Response {
    let mark = if okay { "✓" } else { "!" };
    let js = py::dumps(&json!({"type": "osa-app", "ok": okay}), true);
    let lang = if i18n::lang() == "en" { "en" } else { "zh-CN" };
    let color = if okay { "#5fcf98" } else { "#ef6b6b" };
    let delay = if okay { 1200 } else { 4000 };
    let (title, detail) = (escape(title), escape(detail));
    let html = format!(
        r#"<!doctype html><html lang="{lang}"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title>
<style>body{{margin:0;height:100vh;display:grid;place-items:center;font-family:-apple-system,"PingFang SC",sans-serif;
background:#0d0d0e;color:#ececec;text-align:center}}@media(prefers-color-scheme:light){{body{{background:#fbfbfa;color:#18181a}}}}
.m{{font-size:44px;color:{color}}}p{{margin:6px 24px}}.d{{opacity:.6;font-size:14px}}</style>
<div><div class="m">{mark}</div><p>{title}</p><p class="d">{detail}</p></div>
<script>try{{opener&&opener.postMessage({js},location.origin)}}catch(e){{}}
setTimeout(function(){{if(opener)close();else location.replace('/apps')}},{delay})</script></html>"#
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

async fn oauth_start(Path(name): Path<String>, h: HeaderMap) -> Response {
    let site = h.get("sec-fetch-site").and_then(|v| v.to_str().ok()).unwrap_or("same-origin");
    if site != "same-origin" && site != "none" {
        return ApiError::new(403, t!("请从应用市场里点连接")).into_response();
    }
    if mcp_hub::get(&name).is_none() {
        let r = catalog::build(&name, &json!({})).and_then(|mut cfg| {
            cfg["enabled"] = json!(true);
            mcp_hub::save(&name, &cfg)
        });
        if let Err(e) = r {
            return oauth_page(false, &t!("没法连接"), &e);
        }
    }
    let a = mcp_hub::get(&name).unwrap_or(json!({}));
    if !py::truthy(&a["oauth"]) {
        return oauth_page(false, &t!("这个应用不用登录授权"), &t!("回到应用市场里直接连接就行"));
    }
    match mcp_oauth::login(&name, &callback_url(&h)).await {
        Ok((kind, val)) if kind == "url" => {
            let mut r = StatusCode::FOUND.into_response();
            if let Ok(v) = HeaderValue::from_str(&val) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Ok((_, val)) => {
            let okay = mcp_hub::is_connected(&name);
            let title = if okay { t!("已连接 {title}", title = catalog::title(&name)) } else { t!("没有连上") };
            oauth_page(okay, &title, if okay { "" } else { &val })
        }
        Err(e) => ApiError::bad(e).into_response(),
    }
}

async fn oauth_callback(q: Q) -> Response {
    let g = |k: &str| qs(&q, k).unwrap_or("").to_string();
    let desc = g("error_description");
    let error = if desc.is_empty() { g("error") } else { desc };
    let Some(name) = mcp_oauth::finish_login(&g("state"), &g("code"), &g("iss"), &error).await else {
        return oauth_page(false, &t!("这次授权已经失效"), &t!("回到应用市场重新点一次连接"));
    };
    if mcp_hub::is_connected(&name) {
        return oauth_page(true, &t!("已连接 {title}", title = catalog::title(&name)), &t!("可以关掉这个窗口了"));
    }
    oauth_page(false, &t!("没有连上"), &mcp_hub::status_text(&name))
}

// ---------- 确认规则、保险库 ----------
async fn list_rules() -> Res {
    ok(json!(approvals::rules()))
}

async fn add_rule(Body(d): Body) -> Res {
    let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
    ok(approvals::add_rule(&s("pattern"), &s("action"), &s("note"))?)
}

async fn delete_rule(Path(rid): Path<i64>) -> Res {
    approvals::delete_rule(rid);
    ok(json!({"ok": true}))
}

async fn list_vault() -> Res {
    ok(json!(vault::names()))
}

async fn put_vault(Path(name): Path<String>, Body(d): Body) -> Res {
    vault::put(&name, d["value"].as_str().unwrap_or(""))?;
    ok(json!({"ok": true}))
}

async fn delete_vault(Path(name): Path<String>) -> Res {
    vault::delete(&name);
    ok(json!({"ok": true}))
}

// ---------- 应用自带的界面（MCP Apps） ----------
fn mcp_err((code, msg): (u16, String)) -> ApiError {
    ApiError::new(code, msg)
}

async fn mcp_app_view(Path(app): Path<String>, q: Q) -> Response {
    let uri = qs(&q, "uri").unwrap_or("").to_string();
    match mcpapps::view(&app, &uri).await {
        Ok((html, csp)) => {
            let mut r = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
            let hs = r.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&csp) {
                hs.insert("content-security-policy", v);
            }
            hs.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
            hs.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
            hs.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        Err(e) => ApiError::not_found(e).into_response(),
    }
}

async fn mcp_app_call(Path(app): Path<String>, Body(d): Body) -> Res {
    let bot = if d["bot_id"].is_i64() { d["bot_id"].as_i64().and_then(bots::get) } else { None };
    let args = if py::truthy(&d["arguments"]) { d["arguments"].clone() } else { json!({}) };
    let name = if py::truthy(&d["name"]) { py::str_of(&d["name"]) } else { String::new() };
    mcpapps::call(&app, &name, &args, bot.as_ref()).await.map(axum::Json).map_err(mcp_err)
}

async fn mcp_app_read(Path(app): Path<String>, Body(d): Body) -> Res {
    mcpapps::read(&app, d["uri"].as_str().unwrap_or("")).await.map(axum::Json).map_err(mcp_err)
}

pub const MAX_SAY: usize = 4000;

async fn mcp_app_say(Path(app): Path<String>, Body(d): Body) -> Res {
    if mcp_hub::session_of(&app).is_none() {
        return Err(ApiError::not_found(t!("应用 @{name} 没连接", name = app)));
    }
    let bot_id = if d["bot_id"].is_i64() { d["bot_id"].as_i64() } else { None };
    let text = if py::truthy(&d["text"]) { py::str_of(&d["text"]) } else { String::new() };
    send(bot_id, &text, json!({"app": app, "title": format!("@{app}")})).map_err(mcp_err)?;
    ok(json!({"ok": true}))
}

/// 界面替用户给 Bot 发一条消息，发到 Bot 的私聊里（Python 版是 applets.send；小应用那边迁好后可以合到一起）
fn send(bot_id: Option<i64>, text: &str, origin: Value) -> Result<(), (u16, String)> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err((400, t!("消息不能为空")));
    }
    if py::char_len(&text) > MAX_SAY {
        return Err((400, t!("消息太长（最多 {n} 字）", n = MAX_SAY)));
    }
    let bot = bot_id.and_then(bots::get);
    let chat = bot.as_ref().and_then(|b| b["chat_id"].as_i64()).and_then(bots::chat);
    let Some(chat) = chat else { return Err((404, t!("发起它的 Bot 已删除"))) };
    let bot_id = bots::ids_of(&chat).first().copied().unwrap_or(0);
    let cid = chat["id"].as_i64();
    tokio::spawn(async move {
        if let Err(e) = crate::router::runtime(bot_id).handle_user(&text, "", cid, &[], Some(origin.clone())).await {
            tracing::error!("应用界面发消息出错：{e:#}");
            broadcast(json!({"type": "error", "chat_id": cid, "message": t!("出错了：{e}", e = crate::providers::brief(&e))}));
        }
    });
    Ok(())
}
