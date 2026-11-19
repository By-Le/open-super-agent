//! 编进二进制的数据：从 Python 版导出的工具 schema、服务商列表（server/scripts/dump_assets.py 生成）。

use serde_json::Value;
use std::sync::LazyLock;

static SCHEMAS: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(include_str!("../assets/schemas.json")).expect("schemas.json"));

pub fn schemas() -> &'static Value {
    &SCHEMAS
}

/// 注册表里的某个内置工具的 schema
pub fn tool_schema(name: &str) -> Value {
    SCHEMAS["registry"].as_array().and_then(|a| a.iter().find(|t| t["name"] == name)).map(|t| t["schema"].clone()).unwrap_or(Value::Null)
}

/// 注册表里的某个内置工具的导出信息（where、gated、parallel、describe）
pub fn tool_info(name: &str) -> Value {
    SCHEMAS["registry"].as_array().and_then(|a| a.iter().find(|t| t["name"] == name)).cloned().unwrap_or(Value::Null)
}

pub fn router_tools() -> Vec<Value> {
    SCHEMAS["router_tools"].as_array().cloned().unwrap_or_default()
}

pub fn computer_schema(name: &str) -> Option<Value> {
    SCHEMAS["computer_schemas"]
        .as_array()
        .into_iter()
        .chain(SCHEMAS["desk_schemas"].as_array())
        .flatten()
        .find(|s| s["function"]["name"] == name)
        .cloned()
}
