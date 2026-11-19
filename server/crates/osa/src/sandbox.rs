//! 在沙箱里跑一段代码：run_code（后台任务）和 quick_code（对话里算数、换单位、算日期）。对应 osa/sandbox.py（说明见那里）。
//!
//! 和 Python 版不一样的一处：Python 版跑 Python 用的是服务自己的解释器（sys.executable）。Rust 版没有自带 Python，
//! 按这个顺序找：环境变量 OSA_PYTHON → PATH 上的 python3（macOS 上 /usr/bin/python3 只有装了命令行工具才算，不然一跑就弹安装框）
//! → uv 管理的 Python（`uv python find`，没有就第一次用到时 `uv python install` 下载一份）。找到后问它一次
//! sys.prefix / base_prefix / exec_prefix，沙箱里放开读这几个目录，和 Python 版一样。

use crate::registry::{Call, Out};
use crate::{config, py, security, util};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

pub const LANGS: [&str; 3] = ["python", "bash", "node"];
pub const LIMIT: usize = 20000;
pub const CAP: usize = 256 << 10;
pub const TIMEOUT: i64 = 60;
pub const QUICK_TIMEOUT: i64 = 5;
pub const MAX_TIMEOUT: i64 = 600;
pub const MAX_INPUT: usize = 1 << 20;
pub const MAX_FILES: usize = 20;
pub const RUN: &str = ".run";
pub const KEEP_DAYS: u64 = 7;
pub const QUICK_TIMED_OUT: &str = "超时了，改派后台任务";
const PRELUDE: &str = include_str!("prompts/sandbox/prelude.py");

fn ext(lang: &str) -> &'static str {
    match lang {
        "python" => "py",
        "bash" => "sh",
        _ => "js",
    }
}

pub fn exec_mode() -> String {
    security::exec_mode()
}

// ---------- 在哪跑 ----------
/// 能在里面跑代码的电脑（Docker / Instabox），没有返回空。电脑是第 2 步，现在总是空
fn computer_backend() -> String {
    if !config::get_bool("computer.enabled", true) {
        return String::new();
    }
    // TODO（第 2 步）：computer::backend_name() == "instabox"，或 "local" 且 docker_ok()
    String::new()
}

/// 这次在哪跑：computer（Docker / Instabox 电脑里）、host（本机的受限子进程），都不行返回空
pub fn place(quick: bool) -> String {
    if exec_mode() == "off" {
        return String::new();
    }
    if !quick && !computer_backend().is_empty() {
        return "computer".into();
    }
    if host_sandbox().is_empty() { String::new() } else { "host".into() }
}

static PROBED: Mutex<Option<String>> = Mutex::new(None);

/// 这台机器上能用的沙箱：sandbox-exec / bwrap / unshare，都没有返回空。第一次用时真跑一次试试，结果记住
pub fn host_sandbox() -> String {
    let mut p = PROBED.lock();
    if p.is_none() {
        *p = Some(probe());
    }
    p.clone().unwrap_or_default()
}

pub fn kinds() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec!["sandbox-exec"]
    } else if cfg!(target_os = "linux") {
        vec!["bwrap", "unshare"]
    } else {
        vec![]
    }
}

fn which(cmd: &str) -> Option<String> {
    if cmd.contains('/') {
        return Path::new(cmd).is_file().then(|| cmd.to_string());
    }
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let p = d.join(cmd);
        if let Ok(md) = std::fs::metadata(&p) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
            #[cfg(not(unix))]
            if md.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// 装了、而且真能起来（容器里常常不让建命名空间）
pub fn works(kind: &str) -> bool {
    if which(kind).is_none() {
        return false;
    }
    let Ok(d) = tempdir("osa-probe-") else { return false };
    // 能用 Python 就拿 Python 试（和 Python 版一样，顺便看解释器在沙箱里读不读得到）；unshare 的序幕本身要 Python
    let py_exe = python_known();
    let (argv, reads) = match &py_exe {
        Some(p) => (vec![p.clone(), "-c".into(), "pass".into()], reads_of("python", p)),
        None if kind == "unshare" => return false,
        None => (vec!["/bin/sh".into(), "-c".into(), ":".into()], vec![]),
    };
    let ok = match wrap(kind, &argv, &d, &d, &d, false, &reads) {
        Ok(a) => {
            let mut cmd = std::process::Command::new(&a[0]);
            cmd.args(&a[1..]).current_dir(&d).env_clear().envs(env_of(&d)).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
            match cmd.spawn() {
                Ok(c) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let pid = c.id();
                    std::thread::spawn(move || {
                        let _ = tx.send(c.wait_with_output());
                    });
                    match rx.recv_timeout(Duration::from_secs(15)) {
                        Ok(Ok(o)) if o.status.success() => true,
                        Ok(Ok(o)) => {
                            tracing::warn!("{kind} 用不了：{}", py::tail(&String::from_utf8_lossy(&o.stderr), 300));
                            false
                        }
                        _ => {
                            #[cfg(unix)]
                            unsafe {
                                libc::kill(pid as i32, libc::SIGKILL);
                            }
                            false
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("{kind} 起不来：{e}");
                    false
                }
            }
        }
        Err(_) => false,
    };
    let _ = std::fs::remove_dir_all(&d);
    ok
}

fn probe() -> String {
    kinds().into_iter().find(|k| works(k)).map(String::from).unwrap_or_default()
}

fn no_sandbox() -> String {
    let s = "这台机器没有沙箱（macOS 的 sandbox-exec、Linux 的 bwrap 或 unshare 都用不了），跑不了代码。".to_string();
    if exec_mode() != "host" {
        return s + "这台服务器只允许在沙箱里执行代码。";
    }
    // TODO（第 2 步）：电脑是用户的 Mac / 本机时，提示改用 computer_shell（见 sandbox.py 的 _no_sandbox）
    s
}

// ---------- 对外 ----------
#[derive(Default, Debug)]
pub struct RunResult {
    pub text: String,
    pub files: Vec<String>,
    pub timed_out: bool,
}

fn res(text: impl Into<String>) -> RunResult {
    RunResult { text: text.into(), ..Default::default() }
}

/// 跑一段代码，返回 (给模型的文字, 产出的文件路径)
pub async fn run(lang: &str, code: &Value, files: &Value, timeout: &Value, net: bool, computer: bool) -> (String, Vec<String>) {
    let r = execute(lang, code, files, timeout, net, computer).await;
    (r.text, r.files)
}

pub async fn execute(lang: &str, code: &Value, files: &Value, timeout: &Value, net: bool, computer: bool) -> RunResult {
    let lang = {
        let l = lang.trim().to_lowercase();
        if l.is_empty() { "python".to_string() } else { l }
    };
    if !LANGS.contains(&lang.as_str()) {
        return res(format!("不支持 {lang}，只能用 {}", LANGS.join(" / ")));
    }
    let Some(code) = code.as_str().filter(|c| !c.trim().is_empty()) else {
        return res("没有要跑的代码");
    };
    let main = format!("main.{}", ext(&lang));
    let inputs = match inputs_of(files, &main) {
        Ok(i) => i,
        Err(e) => return res(e),
    };
    if code.len() + inputs.iter().map(|(_, v)| v.len()).sum::<usize>() > MAX_INPUT {
        return res(format!("代码和文件加起来太大了（最多 {} MB），大文件先存进工作文件夹再在代码里读", MAX_INPUT >> 20));
    }
    let timeout = match timeout {
        Value::Null => TIMEOUT,
        v if !py::truthy(v) => TIMEOUT,
        v => py::to_int(v).unwrap_or(TIMEOUT),
    }
    .clamp(1, MAX_TIMEOUT);
    if exec_mode() == "off" {
        return res("这台服务器关掉了执行代码（config.yaml 的 security.exec: off）");
    }
    let wh = place(!computer);
    if wh.is_empty() {
        return res(no_sandbox());
    }
    let _watch = crate::watchdog::long_call(timeout as f64);
    let r = if wh == "computer" {
        Err("电脑里跑代码要等电脑迁过来（第 2 步）".to_string())
    } else {
        on_host(&lang, code, &inputs, timeout, net).await
    };
    // TODO（第 2 步）：派到用户 Mac 上的任务，把产出的文件传到那台 Mac（sandbox.py 的 _to_mac）
    match r {
        Ok(r) => r,
        Err(e) => res(format!("没跑成：{e}")),
    }
}

/// 带进去的文件 [(相对路径, 文字内容)]（保持给的顺序）
fn inputs_of(files: &Value, main: &str) -> Result<Vec<(String, String)>, String> {
    if !py::truthy(files) {
        return Ok(vec![]);
    }
    let Some(m) = files.as_object() else { return Err("files 要写成 {文件名: 内容}".into()) };
    if m.len() > MAX_FILES {
        return Err(format!("一次最多带 {MAX_FILES} 个文件"));
    }
    let mut out = vec![];
    for (name, content) in m {
        let raw = name.replace('\\', "/");
        let absolute = raw.starts_with('/');
        let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        let p = if absolute { format!("/{}", parts.join("/")) } else if parts.is_empty() { ".".into() } else { parts.join("/") };
        if parts.is_empty() || absolute || parts.contains(&"..") || p == main || p == "." || p.starts_with('-') {
            return Err(format!("文件名不行：{name}（写相对路径，不能有 ..）"));
        }
        let Some(c) = content.as_str() else { return Err(format!("{name} 的内容要是文字")) };
        out.push((p, c.to_string()));
    }
    Ok(out)
}

fn rid() -> String {
    format!("{}-{}", py::strftime("%Y%m%d-%H%M%S"), util::token_hex(3))
}

fn text_of(rc: i64, out: &str, timed_out: bool, timeout: i64, files: &[String], note: &str) -> String {
    let mut out = out.trim_end().to_string();
    let n = py::char_len(&out);
    if n > LIMIT {
        out = format!("{}\n…（输出太长，后面 {} 字没给）", py::head(&out, LIMIT), n - LIMIT);
    }
    let head = if timed_out { format!("超时了：{timeout} 秒内没跑完，已经停掉") } else { format!("退出码 {rc}") };
    let mut text = format!("{head}\n{}", if out.is_empty() { "（没有输出）" } else { &out });
    if !files.is_empty() {
        text += &format!("\n产出的文件：\n{}", files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"));
    }
    if !note.is_empty() {
        text += &format!("\n{note}");
    }
    text
}

// ---------- 解释器 ----------
fn real(p: &str) -> String {
    let p = config::expand_user(Path::new(p));
    let p = std::path::absolute(&p).unwrap_or(p);
    match std::fs::canonicalize(&p) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => {
            // realpath 对不存在的路径也给结果：把存在的前缀解开
            let mut cur = p.clone();
            let mut tail = vec![];
            while let Some(parent) = cur.parent().map(|x| x.to_path_buf()) {
                if let Some(n) = cur.file_name() {
                    tail.push(n.to_os_string());
                }
                if let Ok(r) = std::fs::canonicalize(&parent) {
                    let mut out = r;
                    for n in tail.iter().rev() {
                        out.push(n);
                    }
                    return out.to_string_lossy().into_owned();
                }
                cur = parent;
            }
            p.to_string_lossy().into_owned()
        }
    }
}

static PYTHON: Mutex<Option<String>> = Mutex::new(None);
static PREFIXES: LazyLock<Mutex<std::collections::HashMap<String, Vec<String>>>> = LazyLock::new(Default::default);

fn macos_stub(p: &str) -> bool {
    // /usr/bin/python3 只是个壳：没装命令行工具时一跑就弹安装框
    cfg!(target_os = "macos")
        && real(p) == "/usr/bin/python3"
        && !std::process::Command::new("xcode-select").arg("-p").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn uv_find() -> Option<String> {
    let uv = which("uv")?;
    let o = std::process::Command::new(uv).args(["python", "find"]).stderr(std::process::Stdio::null()).output().ok()?;
    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !p.is_empty() && Path::new(&p).is_file()).then_some(p)
}

/// 不下载就能找到的 Python
fn python_known() -> Option<String> {
    if let Some(p) = PYTHON.lock().clone() {
        return Some(p);
    }
    let found = std::env::var("OSA_PYTHON")
        .ok()
        .filter(|p| !p.is_empty())
        .and_then(|p| which(&p))
        .or_else(|| which("python3").filter(|p| !macos_stub(p)))
        .or_else(uv_find);
    if let Some(p) = &found {
        *PYTHON.lock() = Some(p.clone());
    }
    found
}

/// 找 Python，没有就用 uv 装一份（第一次会下载，几十秒）
async fn python() -> Option<String> {
    if let Some(p) = tokio::task::spawn_blocking(python_known).await.ok().flatten() {
        return Some(p);
    }
    let uv = which("uv")?;
    let ok = tokio::process::Command::new(uv).args(["python", "install"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().await.ok()?.success();
    if !ok {
        return None;
    }
    tokio::task::spawn_blocking(python_known).await.ok().flatten()
}

async fn interpreter(lang: &str) -> String {
    match lang {
        "python" => python().await.unwrap_or_default(),
        "bash" => which("bash").unwrap_or_else(|| if Path::new("/bin/bash").exists() { "/bin/bash".into() } else { String::new() }),
        _ => which(lang).unwrap_or_default(),
    }
}

/// 解释器自己的目录：藏起来的地方里装的解释器也要能读
fn reads_of(lang: &str, exe: &str) -> Vec<String> {
    if exe.is_empty() {
        return vec![];
    }
    let mut dirs: Vec<String> = vec![];
    let parent2 = Path::new(&real(exe)).parent().and_then(|p| p.parent()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    if lang == "python" {
        let cached = PREFIXES.lock().get(exe).cloned();
        let pre = cached.unwrap_or_else(|| {
            let o = std::process::Command::new(exe)
                .args(["-I", "-c", "import sys;print(sys.prefix);print(sys.base_prefix);print(sys.exec_prefix)"])
                .stderr(std::process::Stdio::null())
                .output();
            let v: Vec<String> = o.map(|o| String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect()).unwrap_or_default();
            PREFIXES.lock().insert(exe.to_string(), v.clone());
            v
        });
        dirs.extend(pre);
    }
    dirs.push(parent2);
    let mut out: Vec<String> = dirs.iter().filter(|d| !d.is_empty()).map(|d| real(d)).filter(|d| d != "/").collect::<HashSet<_>>().into_iter().collect();
    out.sort();
    out
}

fn config_file() -> String {
    real(&config::ROOT.join(std::env::var("OSA_CONFIG").unwrap_or_else(|_| "config.yaml".into())).to_string_lossy())
}

const SYSTEM: [&str; 10] = ["usr", "bin", "sbin", "lib", "lib32", "lib64", "libx32", "dev", "proc", "sys"];

fn hidden() -> Vec<String> {
    let mine = vec![real("~"), real(&config::ROOT.to_string_lossy()), real(&config::DATA.to_string_lossy()), config_file()];
    if cfg!(target_os = "macos") {
        let mut v: Vec<String> = ["/Users", "/Volumes", "/private/tmp", "/private/var/folders"].iter().map(|s| s.to_string()).collect();
        v.extend(mine);
        return v;
    }
    let top: Vec<String> = match std::fs::read_dir("/") {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !SYSTEM.contains(&n.as_str()) && n != "etc").map(|n| format!("/{n}")).collect(),
        Err(_) => ["/root", "/home", "/opt", "/srv", "/mnt", "/media", "/var", "/run", "/boot"].iter().map(|s| s.to_string()).collect(),
    };
    let mut s: Vec<String> = std::iter::once("/tmp".to_string()).chain(top).collect::<HashSet<_>>().into_iter().collect();
    s.sort();
    s.extend(mine);
    s
}

const ETC_KEEP: [&str; 21] = [
    "passwd", "group", "nsswitch.conf", "hosts", "resolv.conf", "localtime", "timezone", "ld.so.cache", "ld.so.conf", "ld.so.conf.d", "ssl",
    "ca-certificates", "pki", "alternatives", "mime.types", "fonts", "os-release", "lsb-release", "debian_version", "python3", "matplotlibrc",
];

static SECRET: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?i)KEY|TOKEN|SECRET|PASSW|CREDENTIAL|^OSA_").unwrap());

/// 环境变量：去掉密钥和服务自己的设置（同 computer/host.py 的 _env），再换上沙箱的 HOME 和临时目录
pub fn env_of(tmp: &Path) -> Vec<(String, String)> {
    let tmp = tmp.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| !SECRET.is_match(k)).collect();
    let lang = std::env::var("LANG").ok().filter(|l| !l.is_empty()).unwrap_or_else(|| "C.UTF-8".into());
    for (k, v) in [("HOME", tmp.clone()), ("TMPDIR", tmp), ("PYTHONUTF8", "1".into()), ("PYTHONDONTWRITEBYTECODE", "1".into()), ("PYTHONUNBUFFERED", "1".into()), ("MPLBACKEND", "Agg".into()), ("LANG", lang)] {
        env.retain(|(x, _)| x != k);
        env.push((k.to_string(), v));
    }
    env
}

fn under(p: &str, roots: &[String]) -> bool {
    roots.iter().any(|r| p == r || p.starts_with(&format!("{}/", r.trim_end_matches('/'))))
}

fn sbpl(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn seatbelt(ws: &str, tmp: &str, reads: &[String], net: bool) -> String {
    let wh = |paths: &[String]| paths.iter().map(|p| if Path::new(p).is_file() { format!("(literal {})", sbpl(p)) } else { format!("(subpath {})", sbpl(p)) }).collect::<Vec<_>>().join(" ");
    let hide: Vec<String> = hidden().into_iter().filter(|h| Path::new(h).exists()).collect();
    let mut allow: Vec<String> = reads.to_vec();
    allow.push(ws.into());
    allow.push(tmp.into());
    let mut rules = vec![
        "(version 1)".to_string(),
        "(deny default)".into(),
        "(allow process-exec process-fork)".into(),
        "(allow signal (target same-sandbox))".into(),
        "(allow process-info* (target same-sandbox))".into(),
        "(allow sysctl-read)".into(),
        "(allow ipc-posix-sem)".into(),
        "(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))".into(),
        "(allow file-read*)".into(),
        format!("(deny file-read* {})", wh(&hide)),
        format!("(allow file-read* {})", wh(&allow)),
        "(allow file-read-metadata)".into(),
        format!("(allow file-write* {})", wh(&[ws.to_string(), tmp.to_string()])),
        "(allow file-write-data file-ioctl (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\") (subpath \"/dev/fd\"))".into(),
    ];
    if net {
        rules.extend([
            "(allow network-outbound (remote ip))".to_string(),
            "(deny network-outbound (remote ip \"localhost:*\"))".into(),
            "(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))".into(),
            "(allow system-socket)".into(),
            "(allow mach-lookup (global-name \"com.apple.dnssd.service\") (global-name \"com.apple.SystemConfiguration.configd\") (global-name \"com.apple.SystemConfiguration.DNSConfiguration\") (global-name \"com.apple.trustd.agent\"))".into(),
        ]);
    }
    rules.join("\n")
}

fn is_symlink(p: &str) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// Linux 沙箱里看到的文件系统：(换成空目录的, 只读带进来的, 能写的)
fn view(ws: &str, reads: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut cands: Vec<String> = hidden().iter().map(|x| real(x)).collect::<HashSet<_>>().into_iter().collect();
    cands.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    let mut hide: Vec<String> = vec![];
    for h in cands {
        if h != "/" && Path::new(&h).is_dir() && !is_symlink(&h) && !under(&h, &hide) {
            hide.push(h);
        }
    }
    hide.push("/etc".into());
    let mut ro: Vec<String> = ETC_KEEP.iter().map(|f| format!("/etc/{f}")).filter(|p| Path::new(p).exists()).collect();
    ro.extend(reads.iter().filter(|r| Path::new(r).exists() && under(r, &hide) && !under(r, &[ws.to_string()])).cloned());
    let cfg = config_file();
    if Path::new(&cfg).is_file() && !under(&cfg, &hide) {
        ro.push(cfg);
    }
    ro.sort_by_key(|s| s.len());
    (hide, ro, vec![ws.to_string()])
}

fn bwrap(argv: &[String], ws: &str, run: &str, reads: &[String]) -> Vec<String> {
    let (hide, ro, rw) = view(ws, reads);
    let cfg = config_file();
    let mut a: Vec<String> = [
        "bwrap", "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--unshare-user-try", "--unshare-ipc", "--unshare-pid", "--unshare-uts",
        "--unshare-cgroup-try", "--unshare-net", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for h in &hide {
        a.extend(["--tmpfs".into(), h.clone()]);
    }
    for p in &ro {
        if *p == cfg {
            a.extend(["--ro-bind".into(), "/dev/null".into(), p.clone()]);
        } else {
            a.extend(["--ro-bind-try".into(), p.clone(), p.clone()]);
        }
    }
    for p in &rw {
        a.extend(["--bind".into(), p.clone(), p.clone()]);
    }
    for h in &hide {
        if h != "/tmp" {
            a.extend(["--remount-ro".into(), h.clone()]);
        }
    }
    a.extend(["--chdir".into(), run.into(), "--".into()]);
    a.extend(argv.iter().cloned());
    a
}

fn unshare(argv: &[String], ws: &str, run: &str, reads: &[String]) -> Result<Vec<String>, String> {
    let (hide, ro, rw) = view(ws, reads);
    let py = python_known().ok_or("unshare 要用 Python")?;
    let spec = json!({"hide": hide, "ro": ro, "rw": rw, "cfg": config_file(), "cwd": run, "argv": argv});
    let mut a: Vec<String> = ["unshare", "--user", "--map-root-user", "--mount", "--pid", "--fork", "--net", "--ipc", "--uts", "--"].iter().map(|s| s.to_string()).collect();
    a.extend([py, "-I".into(), "-S".into(), "-c".into(), PRELUDE.into(), py::dumps(&spec, true)]);
    Ok(a)
}

fn wrap(kind: &str, argv: &[String], ws: &Path, run: &Path, tmp: &Path, net: bool, reads: &[String]) -> Result<Vec<String>, String> {
    let (ws, run, tmp) = (ws.to_string_lossy(), run.to_string_lossy(), tmp.to_string_lossy());
    match kind {
        "sandbox-exec" => {
            let mut a = vec!["sandbox-exec".to_string(), "-p".into(), seatbelt(&ws, &tmp, reads, net)];
            a.extend(argv.iter().cloned());
            Ok(a)
        }
        "bwrap" => Ok(bwrap(argv, &ws, &run, reads)),
        "unshare" => unshare(argv, &ws, &run, reads),
        k => Err(k.to_string()),
    }
}

fn tempdir(prefix: &str) -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..10 {
        let d = base.join(format!("{prefix}{}", util::token_hex(4)));
        if std::fs::create_dir(&d).is_ok() {
            return Ok(std::fs::canonicalize(&d).unwrap_or(d));
        }
    }
    Err(std::io::Error::other("mkdtemp"))
}

/// 跑起来，stdout 和 stderr 合在一起收（最多 CAP 字节），超时连它开的子进程一起停掉。返回 (退出码, 输出, 是否超时)
async fn spawn(argv: &[String], cwd: &Path, env: Vec<(String, String)>, timeout: i64) -> std::io::Result<(i64, String, bool)> {
    // stdout 和 stderr 写进同一根管子
    let (mut rd, wr) = std::io::pipe()?;
    let wr2 = wr.try_clone()?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).env_clear().envs(env).stdin(std::process::Stdio::null()).stdout(wr).stderr(wr2).kill_on_drop(true);
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(cmd);
    let pid = child.id().map(|p| p as i32);
    let reader = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 65536];
        loop {
            match rd.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < CAP {
                        let take = n.min(CAP - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        buf
    });
    let kill = || {
        #[cfg(unix)]
        if let Some(p) = pid {
            unsafe {
                libc::killpg(p, libc::SIGKILL);
            }
        }
    };
    let waited = tokio::time::timeout(Duration::from_secs(timeout as u64), child.wait()).await;
    let (rc, timed_out) = match waited {
        Ok(Ok(s)) => (code_of(s), false),
        Ok(Err(e)) => {
            kill();
            return Err(e);
        }
        Err(_) => {
            kill();
            let _ = child.kill().await;
            let _ = child.wait().await;
            (-9, true)
        }
    };
    // 子进程退出了，它开的后台进程可能还拿着管子：给一点时间读完，然后连它们一起停掉
    let buf = match tokio::time::timeout(Duration::from_secs(2), reader).await {
        Ok(Ok(b)) => b,
        _ => {
            kill();
            Vec::new()
        }
    };
    Ok((rc, String::from_utf8_lossy(&buf).into_owned(), timed_out))
}

fn code_of(s: std::process::ExitStatus) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return -(sig as i64);
        }
    }
    s.code().unwrap_or(-1) as i64
}

fn prune(root: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(KEEP_DAYS * 86400);
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.filter_map(|e| e.ok()) {
        if let Ok(md) = std::fs::symlink_metadata(e.path()) {
            if md.is_dir() && md.modified().is_ok_and(|m| m < old) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

fn outputs(run: &Path, skip: &HashSet<String>) -> Vec<String> {
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&p, base, out);
            } else {
                // os.walk 不进符号链接的目录，文件（含链接）都算
                out.push(p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = vec![];
    walk(run, run, &mut out);
    out.retain(|r| !skip.contains(r));
    out.sort();
    out.truncate(50);
    out
}

async fn on_host(lang: &str, code: &str, inputs: &[(String, String)], timeout: i64, net: bool) -> Result<RunResult, String> {
    let kind = host_sandbox();
    let exe = interpreter(lang).await;
    if exe.is_empty() {
        return Ok(res(format!("这台机器上没有 {lang}，换一种语言写吧")));
    }
    let ws = security::workspace();
    std::fs::create_dir_all(&ws).map_err(|e| e.to_string())?;
    let ws = PathBuf::from(real(&ws.to_string_lossy()));
    prune(&ws.join(RUN));
    let main = format!("main.{}", ext(lang));
    let run = ws.join(RUN).join(rid());
    let mut all: Vec<(String, String)> = inputs.to_vec();
    all.push((main.clone(), code.to_string()));
    for (name, content) in &all {
        let p = run.join(name);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, content).map_err(|e| e.to_string())?;
    }
    let tmp = if kind == "sandbox-exec" { tempdir("osa-run-").map_err(|e| e.to_string())? } else { PathBuf::from("/tmp") };
    let argv: Vec<String> = if lang == "python" { vec![exe.clone(), "-B".into(), main.clone()] } else { vec![exe.clone(), main.clone()] };
    let reads = { let (l, e) = (lang.to_string(), exe.clone()); tokio::task::spawn_blocking(move || reads_of(&l, &e)).await.unwrap_or_default() };
    let r = match wrap(&kind, &argv, &ws, &run, &tmp, net, &reads) {
        Ok(a) => spawn(&a, &run, env_of(&tmp), timeout).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    if kind == "sandbox-exec" {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    let (rc, out, timed_out) = r?;
    let _ = std::fs::remove_file(run.join(&main));
    let skip: HashSet<String> = inputs.iter().map(|(n, _)| n.clone()).collect();
    let made = outputs(&run, &skip);
    if made.is_empty() {
        let _ = std::fs::remove_dir_all(&run);
    }
    let files: Vec<String> = made.iter().map(|x| run.join(x).to_string_lossy().into_owned()).collect();
    Ok(RunResult { text: text_of(rc, &out, timed_out, timeout, &files, ""), files, timed_out })
}

// ---------- 工具 ----------
pub fn progress(a: &Value) -> String {
    let code = crate::builtin::arg_s(a, "code");
    let first = code.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
    t!("跑代码：{what}", what = py::head(first, 60))
}

pub async fn tool_run(c: Call) -> Out {
    let a = &c.args;
    let lang = if py::truthy(&a["lang"]) { py::str_of(&a["lang"]) } else { "python".into() };
    let (text, _) = run(&lang, &a["code"], &a["files"], &a["timeout"], true, true).await;
    Out::Text(text)
}

pub async fn tool_quick(c: Call) -> Out {
    let r = execute("python", &c.args["code"], &Value::Null, &json!(QUICK_TIMEOUT), false, false).await;
    Out::Text(if r.timed_out { QUICK_TIMED_OUT.into() } else { r.text })
}
