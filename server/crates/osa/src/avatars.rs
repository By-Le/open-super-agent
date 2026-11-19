//! 头像图片（待迁：osa/avatars.py）。
pub fn migrate() {
    crate::db::batch("create table if not exists avatars(id text primary key, mime text, data blob, created real)");
}
pub fn valid(url: &str) -> bool {
    let id = url.strip_prefix("/avatars/").unwrap_or("");
    !id.is_empty() && crate::db::exists("select 1 from avatars where id=?", rusqlite::params![id])
}
pub fn gc() {}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
