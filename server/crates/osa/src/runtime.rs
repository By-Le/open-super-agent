//! 用到时再下载的运行环境：uv（uvx 应用要它，没有 Python 时也靠它装一份）和 uv 管的 Python。
//!
//! 桌面 App 不再带 105 MB 的 Python 运行环境，设了 OSA_RUNTIME_DIR（App 放在它自己的目录下）。
//! PATH 上没有 uv 时，第一次用到就下载固定版本的 uv / uvx 到 $OSA_RUNTIME_DIR/bin：取的是 PyPI 上 uv 的安装包（wheel），
//! 按写死的 sha256 校验。没设 OSA_RUNTIME_DIR（Docker、自己部署）就不下载，和原来一样只找 PATH。
//!
//! 国内网络：OSA_MIRROR=cn 时 uv 从阿里云的 PyPI 镜像下载，并且（没单独设的话）给 uv、npm 都换上国内镜像——
//! uvx 应用装包、uv 下载 Python、npx 应用装包都走国内。桌面 App 第一次打开时测一下网络自动选，设置里能改。
//!
//! `osa-server setup`：桌面 App 第一次打开时调它，把 uv 和 Python 提前装好，每一步一行 JSON 报进度。

use std::path::{Path, PathBuf};

const UV_VERSION: &str = "0.13.6";
const PYTHON_VERSION: &str = "3.13";

/// PyPI 上 uv 安装包的路径（/packages/ 后面那段，镜像和官方一样）、sha256、大小
fn wheel() -> Option<(&'static str, &'static str, u64)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some((
            "f4/14/77230cc4f6a73a31578cb8070a5f67f58a76946ead643ae8a4155620d6da/uv-0.13.6-py3-none-macosx_11_0_arm64.whl",
            "51361029b7cf91185d0092c4b356d110b4ad629e9cb11593f1e006ac72a72ca9",
            14344205,
        )),
        ("macos", "x86_64") => Some((
            "33/55/ff10d3f756c39a239e492e57262bd22fcda412a9f85a268175447cf33b09/uv-0.13.6-py3-none-macosx_10_12_x86_64.whl",
            "95f042b95e67ef8c3d3569bb6e9d895e592002c5e871cf3f1afe407c9d77a294",
            17172613,
        )),
        _ => None,
    }
}

/// 国内镜像
pub const CN_PYPI_FILES: &str = "https://mirrors.aliyun.com/pypi/packages";
pub const CN_PYPI_INDEX: &str = "https://mirrors.aliyun.com/pypi/simple";
pub const CN_PYTHON: &str = "https://registry.npmmirror.com/-/binary/python-build-standalone";
pub const CN_NPM: &str = "https://registry.npmmirror.com";

pub fn china() -> bool {
    std::env::var("OSA_MIRROR").is_ok_and(|v| v.trim().eq_ignore_ascii_case("cn"))
}

/// 下载的东西放哪；没设就不下载
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("OSA_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn bin_dir() -> Option<PathBuf> {
    dir().map(|d| d.join("bin"))
}

/// 启动时调（main 一开始、还没起别的线程）：把 $OSA_RUNTIME_DIR/bin 放到 PATH 最前面，下载过的 uv、uvx 应用和沙箱都能直接找到；
/// OSA_MIRROR=cn 时给 uv、npm 换上国内镜像（已经单独设了的不动）。子进程（应用、沙箱、setup）都继承这些环境变量。
pub fn init_env() {
    let mut set: Vec<(&str, std::ffi::OsString)> = vec![];
    if let Some(bin) = bin_dir() {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut parts: Vec<PathBuf> = vec![bin.clone()];
        parts.extend(std::env::split_paths(&old).filter(|p| *p != bin));
        if let Ok(p) = std::env::join_paths(parts) {
            set.push(("PATH", p));
        }
    }
    if china() {
        for (k, v) in [
            ("UV_DEFAULT_INDEX", CN_PYPI_INDEX),
            ("UV_PYTHON_INSTALL_MIRROR", CN_PYTHON),
            ("npm_config_registry", CN_NPM),
        ] {
            if std::env::var_os(k).is_none() {
                set.push((k, v.into()));
            }
        }
    }
    for (k, v) in set {
        // SAFETY: main 一开始、还没有别的线程时调用
        unsafe { std::env::set_var(k, v) };
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
    uv_with(|_, _| {}).await.ok()
}

/// 同上，下载时报进度（已下载、总共的字节数）；失败时返回原因
pub async fn uv_with(progress: impl Fn(u64, u64)) -> Result<String, String> {
    if let Some(p) = which("uv") {
        return Ok(p.to_string_lossy().into_owned());
    }
    let bin = bin_dir().ok_or("PATH 上没有 uv")?;
    let _g = LOCK.lock().await;
    let exe = bin.join("uv");
    if !exe.is_file() {
        download(&bin, &progress).await.map_err(|e| {
            tracing::warn!("下载 uv 失败：{e}");
            format!("下载 uv 失败：{e}")
        })?;
        tracing::info!("下载好了 uv {UV_VERSION}：{}", bin.display());
    }
    Ok(exe.to_string_lossy().into_owned())
}

async fn download(bin: &Path, progress: &impl Fn(u64, u64)) -> anyhow::Result<()> {
    use futures::StreamExt;
    use sha2::Digest;
    let (path, sha, size) = wheel().ok_or_else(|| anyhow::anyhow!("这个系统没有可下载的 uv"))?;
    let base = if china() { CN_PYPI_FILES } else { "https://files.pythonhosted.org/packages" };
    let url = format!("{base}/{path}");
    // 阿里云的镜像不认没有 User-Agent 的请求（403）
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("osa-server/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(size);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut stream = resp.bytes_stream();
    progress(0, total);
    while let Some(chunk) = tokio::time::timeout(std::time::Duration::from_secs(60), stream.next())
        .await
        .map_err(|_| anyhow::anyhow!("下载卡住了"))?
    {
        bytes.extend_from_slice(&chunk?);
        progress(bytes.len() as u64, total);
    }
    let got: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    anyhow::ensure!(got == sha, "校验不对（{got}）");
    let bin = bin.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&bin)?;
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        // 安装包里是 uv-<版本>.data/scripts/uv、uvx；先放 uvx，最后放 uv（有 uv 就算装好了），都是先写临时文件再改名
        for name in ["uvx", "uv"] {
            let mut f = z.by_name(&format!("uv-{UV_VERSION}.data/scripts/{name}"))?;
            let tmp = bin.join(format!(".{name}.{}", std::process::id()));
            let mut out = std::fs::File::create(&tmp)?;
            std::io::copy(&mut f, &mut out)?;
            drop(out);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&tmp, bin.join(name))?;
        }
        Ok(())
    })
    .await??;
    Ok(())
}

/// uv 管的 Python 装好了没有（`uv python find`，只认 uv 自己下载的）
async fn python_installed(uv: &str) -> Option<String> {
    let o = tokio::process::Command::new(uv)
        .args(["python", "find", "--managed-python", PYTHON_VERSION])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !p.is_empty()).then_some(p)
}

/// `osa-server setup`：装好 uv 和 Python，每一步一行 JSON 打到标准输出：
///   {"step":"uv","status":"running","done":字节,"total":字节}  {"step":"uv","status":"done"}
///   {"step":"python","status":"running"}  {"step":"python","status":"done","path":"…"}
///   出错：{"step":…,"status":"error","message":"…"}，退出码 1
pub async fn setup_main() -> anyhow::Result<()> {
    use std::io::Write;
    let say = |v: serde_json::Value| {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    let last = std::sync::atomic::AtomicU64::new(u64::MAX);
    let uv = uv_with(|done, total| {
        // 每 1% 报一次就够了
        let pct = if total > 0 { done * 100 / total } else { 0 };
        if last.swap(pct, std::sync::atomic::Ordering::Relaxed) != pct {
            say(serde_json::json!({"step": "uv", "status": "running", "done": done, "total": total}));
        }
    })
    .await;
    let uv = match uv {
        Ok(u) => u,
        Err(e) => {
            say(serde_json::json!({"step": "uv", "status": "error", "message": e}));
            std::process::exit(1);
        }
    };
    say(serde_json::json!({"step": "uv", "status": "done"}));
    if let Some(p) = python_installed(&uv).await {
        say(serde_json::json!({"step": "python", "status": "done", "path": p}));
        return Ok(());
    }
    say(serde_json::json!({"step": "python", "status": "running"}));
    let o = tokio::process::Command::new(&uv)
        .args(["python", "install", PYTHON_VERSION])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;
    match python_installed(&uv).await {
        Some(p) if o.status.success() => say(serde_json::json!({"step": "python", "status": "done", "path": p})),
        _ => {
            let err = String::from_utf8_lossy(&o.stderr);
            let msg: String = err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("uv python install 失败").into();
            say(serde_json::json!({"step": "python", "status": "error", "message": msg}));
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// 真的去下载：cargo test --lib runtime -- --ignored（OSA_MIRROR=cn 走国内镜像）
    #[tokio::test]
    #[ignore]
    async fn downloads_uv() {
        let d = std::env::temp_dir().join(format!("osa-rt-{}", std::process::id()));
        unsafe {
            std::env::set_var("OSA_RUNTIME_DIR", &d);
            std::env::set_var("PATH", "/usr/bin:/bin");
        }
        let seen = std::sync::atomic::AtomicU64::new(0);
        let uv = super::uv_with(|done, _| seen.store(done, std::sync::atomic::Ordering::Relaxed)).await.expect("uv");
        assert!(seen.load(std::sync::atomic::Ordering::Relaxed) > 1_000_000);
        let o = std::process::Command::new(&uv).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&o.stdout).contains(super::UV_VERSION));
        assert!(d.join("bin/uvx").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }
}
