//! 用到时再下载的运行环境：uv（uvx 应用要它，没有 Python 时也靠它装一份）。
//!
//! 桌面 App 不再带 105 MB 的 Python 运行环境，设了 OSA_RUNTIME_DIR（App 放在它自己的目录下）。
//! PATH 上没有 uv 时，第一次用到就从 GitHub 下载固定版本的 uv / uvx 到 $OSA_RUNTIME_DIR/bin，按写死的 sha256 校验。
//! 没设 OSA_RUNTIME_DIR（Docker、自己部署）就不下载，和原来一样只找 PATH。

use std::path::{Path, PathBuf};

const UV_VERSION: &str = "0.13.6";

fn asset() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some(("aarch64-apple-darwin", "60fbd5c8a1ed681845eea28d9d622fcde03c51e91a43465b64a1e17db1ed3472")),
        ("macos", "x86_64") => Some(("x86_64-apple-darwin", "63ba17fdda55ad1a230d4b37fc6017ffa10c23688c83818bd3bfdf914c652a02")),
        _ => None,
    }
}

/// 下载的东西放哪；没设就不下载
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("OSA_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn bin_dir() -> Option<PathBuf> {
    dir().map(|d| d.join("bin"))
}

/// 启动时把 $OSA_RUNTIME_DIR/bin 放到 PATH 最前面：下载过的 uv、uvx 应用和沙箱都能直接找到。
/// 要在起别的线程之前调（main 一开始）。
pub fn init_path() {
    let Some(bin) = bin_dir() else { return };
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut parts: Vec<PathBuf> = vec![bin.clone()];
    parts.extend(std::env::split_paths(&old).filter(|p| *p != bin));
    if let Ok(p) = std::env::join_paths(parts) {
        // SAFETY: main 一开始、还没有别的线程时调用
        unsafe { std::env::set_var("PATH", p) };
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(cmd)).find(|p| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    })
}

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 找 uv，没有就下载（几秒到几十秒）。找不到也下载不了返回 None
pub async fn uv() -> Option<String> {
    if let Some(p) = which("uv") {
        return Some(p.to_string_lossy().into_owned());
    }
    let bin = bin_dir()?;
    let _g = LOCK.lock().await;
    let exe = bin.join("uv");
    if !exe.is_file() {
        match download(&bin).await {
            Ok(()) => tracing::info!("下载好了 uv {UV_VERSION}：{}", bin.display()),
            Err(e) => {
                tracing::warn!("下载 uv 失败：{e}");
                return None;
            }
        }
    }
    exe.is_file().then(|| exe.to_string_lossy().into_owned())
}

async fn download(bin: &Path) -> anyhow::Result<()> {
    use sha2::Digest;
    let (triple, sha) = asset().ok_or_else(|| anyhow::anyhow!("这个系统没有可下载的 uv"))?;
    let url = format!("https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/uv-{triple}.tar.gz");
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300)).build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let got: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    anyhow::ensure!(got == sha, "校验不对（{got}）");
    let bin = bin.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&bin)?;
        let tmp = bin.join(format!(".uv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        untar(gz, &tmp)?;
        // 包里是 uv-<triple>/uv、uv-<triple>/uvx；先放 uvx，最后放 uv（有 uv 就算装好了）
        for name in ["uvx", "uv"] {
            let src = tmp.join(format!("uv-{triple}")).join(name);
            anyhow::ensure!(src.is_file(), "包里没有 {name}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&src, bin.join(name))?;
        }
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(())
    })
    .await??;
    Ok(())
}

/// 只认普通文件、不许 .. 和绝对路径的极简 tar 解包（uv 的发布包就是这样）
fn untar(mut r: impl std::io::Read, to: &Path) -> anyhow::Result<()> {
    let mut h = [0u8; 512];
    loop {
        if r.read_exact(&mut h).is_err() || h.iter().all(|b| *b == 0) {
            return Ok(());
        }
        let field = |a: usize, b: usize| String::from_utf8_lossy(&h[a..b]).trim_end_matches('\0').trim().to_string();
        let mut name = field(0, 100);
        let prefix = field(345, 500);
        if !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        let size = usize::from_str_radix(&field(124, 136), 8).unwrap_or(0);
        let kind = h[156];
        let mut data = vec![0u8; size.div_ceil(512) * 512];
        r.read_exact(&mut data)?;
        data.truncate(size);
        let rel = Path::new(&name);
        let safe = !name.is_empty() && rel.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
        if (kind == b'0' || kind == 0) && safe {
            let p = to.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(p, data)?;
        }
    }
}

#[cfg(test)]
mod tests {
    /// 真的去 GitHub 下载：cargo test --lib runtime -- --ignored
    #[tokio::test]
    #[ignore]
    async fn downloads_uv() {
        let d = std::env::temp_dir().join(format!("osa-rt-{}", std::process::id()));
        unsafe {
            std::env::set_var("OSA_RUNTIME_DIR", &d);
            std::env::set_var("PATH", "/usr/bin:/bin");
        }
        let uv = super::uv().await.expect("uv");
        let o = std::process::Command::new(&uv).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&o.stdout).contains(super::UV_VERSION));
        assert!(d.join("bin/uvx").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }
}
