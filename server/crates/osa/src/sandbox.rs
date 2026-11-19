//! run_code 沙箱（待迁：osa/sandbox.py）。
use crate::registry::{Call, Out};
use serde_json::Value;
/// 这次在哪跑：computer / host，都不行返回空
pub fn place(_quick: bool) -> String {
    String::new()
}
pub fn progress(a: &Value) -> String {
    let code = crate::builtin::arg_s(a, "code");
    let first = code.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
    crate::t!("跑代码：{what}", what = crate::py::head(first, 60))
}
pub async fn tool_run(_c: Call) -> Out {
    crate::builtin::todo("run_code")
}
pub async fn tool_quick(_c: Call) -> Out {
    crate::builtin::todo("quick_code")
}
