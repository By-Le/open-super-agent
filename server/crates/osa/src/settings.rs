//! 界面上可修改的设置（迁移中：osa/settings.py）。
use serde_json::{Value, json};
pub fn apply() {
    crate::providers::apply();
}
pub fn onboarding() -> bool {
    if crate::db::exists("select 1 from settings where key='onboarding'", []) {
        return false;
    }
    !crate::db::exists("select 1 from messages where role='user' limit 1", [])
}
pub fn finish_onboarding() {
    crate::memory::set_setting("onboarding", &json!({"done": crate::py::now()}));
}
pub fn public() -> Value {
    json!({"language": crate::i18n::lang()})
}
pub async fn update(_d: &Value) -> Result<Value, String> {
    Err("Rust 版还没有这个功能：改设置".into())
}
