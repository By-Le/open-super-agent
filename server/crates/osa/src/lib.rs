//! Open Super Agent 的服务端（Rust 版）。模块和 osa/*.py 一一对应，名字不变；对外的接口、WebSocket 消息、
//! 数据库表结构和 Python 版一样（见 docs/plans/rust-rewrite.md）。

pub mod py;
pub mod util;
pub mod config;
pub mod db;
#[macro_use]
pub mod i18n;
pub mod assets;
pub mod web;

pub mod approvals;
pub mod apps;
pub mod observe;
pub mod outputs;
pub mod unfurl;
pub mod templates;
pub mod sources;
pub mod sandbox;
pub mod videos;
pub mod images;
pub mod auth;
pub mod avatars;
pub mod backup;
pub mod botadmin;
pub mod bots;
pub mod builtin;
pub mod catalog;
pub mod channels;
pub mod computer;
pub mod dm;
pub mod dsml;
pub mod groups;
pub mod library;
pub mod mcp_client;
pub mod mcp_hub;
pub mod mcp_oauth;
pub mod mcpapps;
pub mod media;
pub mod memories;
pub mod memory;
pub mod prices;
pub mod providers;
pub mod push;
pub mod registry;
pub mod router;
pub mod routes;
pub mod routines;
pub mod search;
pub mod security;
pub mod server;
pub mod settings;
pub mod skills;
pub mod task;
pub mod tasklog;
pub mod teach;
pub mod tools;
pub mod vault;
pub mod watchdog;
pub mod worker;

/// 各模块自己的表（Python 版在 import 时建）
pub fn modules_migrate() {
    avatars::migrate();
    mcp_hub::migrate();
    media::migrate();
}

/// 后台循环（清理、刷新价格……）
pub fn background() {
    tokio::spawn(media::gc_loop());
    tokio::spawn(prices::price_loop());
}
