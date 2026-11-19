//! 资料库（待迁：osa/library.py）。
use crate::util::normpath;
/// 电脑里的路径：相对路径按工作文件夹算，化掉 ..
pub fn norm(path: &str, workspace: Option<&str>) -> Result<String, String> {
    let p = path.trim();
    if p.is_empty() {
        return Err(crate::t!("路径不能为空"));
    }
    let ws = workspace.unwrap_or("/workspace");
    let full = if p.starts_with('/') { p.to_string() } else { format!("{}/{p}", ws.trim_end_matches('/')) };
    Ok(normpath(&full))
}
