//! NOS Web 端「独立服务」的生命周期控制。
//!
//! NOS 的 Web 端始终是一个单独的 FastAPI/uvicorn 进程，岛仔不内嵌它的代码、也不复制它的
//! 数据模型。本模块只负责：按配置拉起本机进程、注入 `NOS_API_TOKEN`、把输出落到日志、
//! 记录 PID、探测端口与 `/api/daozai/ping`、以及停止进程树。
//!
//! 服务起来之后，课表数据仍由 `nos.rs` 通过 HTTP 读取；两层互不依赖对方的实现细节。
//! 托盘「退出」走的是 `ExitProcess`，Rust 的 `Drop` 不会执行，所以 Windows 下额外把子进程
//! 放进带 `KILL_ON_JOB_CLOSE` 的作业对象：岛仔进程消失时由系统收走 uvicorn，不留后台残留。

use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8000;
pub const PID_FILE: &str = "nos-server.pid";
pub const LOG_FILE: &str = "nos-web.log";
const LOG_LIMIT: u64 = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const PING_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------- 配置

/// 启动一台本机 NOS Web 服务所需的全部信息。
///
/// `token` 来自 Windows 凭据管理器（与 `nos.rs` 共用），只作为子进程环境变量注入，
/// 不写进 `settings.txt`。
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// NOS 仓库里的 `web/` 目录（其下应有 `app/main.py`）。
    pub work_dir: String,
    /// Python 可执行文件；留空时自动找 `web/venv` 或 PATH 上的 python。
    pub python: String,
    /// uvicorn 监听地址，默认只监听本机回环。
    pub host: String,
    pub port: u16,
    /// 注入子进程的 `NOS_API_TOKEN`。
    pub token: String,
    /// 日志与 PID 文件目录（一般是 `%APPDATA%/Daozai`）。
    pub log_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            work_dir: String::new(),
            python: String::new(),
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT,
            token: String::new(),
            log_dir: crate::model::config_dir(),
        }
    }
}

impl Config {
    pub fn host(&self) -> &str {
        let host = self.host.trim();
        if host.is_empty() { DEFAULT_HOST } else { host }
    }

    /// 岛仔与浏览器访问的地址。IPv6 字面量要加方括号。
    pub fn base_url(&self) -> String {
        let host = self.host();
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        format!("http://{host}:{}", self.port)
    }

    pub fn dir(&self) -> PathBuf { PathBuf::from(self.work_dir.trim()) }
    pub fn pid_path(&self) -> PathBuf { self.log_dir.join(PID_FILE) }
    pub fn log_path(&self) -> PathBuf { self.log_dir.join(LOG_FILE) }
    pub fn previous_log_path(&self) -> PathBuf { self.log_dir.join("nos-web.previous.log") }

    pub fn python_exe(&self) -> PathBuf { resolve_python(&self.dir(), self.python.trim()) }

    /// `python -m uvicorn app.main:app --host ... --port ...` 的参数部分，单独抽出便于断言。
    pub fn uvicorn_args(&self) -> Vec<String> {
        vec![
            "-m".into(),
            "uvicorn".into(),
            "app.main:app".into(),
            "--host".into(),
            self.host().into(),
            "--port".into(),
            self.port.to_string(),
        ]
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.work_dir.trim().is_empty() {
            return Err("请先填写 NOS Web 目录".into());
        }
        let dir = self.dir();
        if !dir.is_dir() {
            return Err(format!("目录不存在：{}", dir.display()));
        }
        if !dir.join("app").join("main.py").is_file() {
            return Err("该目录下没有 app/main.py，请选择 NOS 仓库的 web 目录".into());
        }
        if self.port == 0 {
            return Err("端口不能为 0".into());
        }
        let host = self.host();
        if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("监听地址包含非法字符".into());
        }
        let exe = self.python_exe();
        if exe.is_absolute() && !exe.is_file() {
            return Err(format!("Python 不存在：{}", exe.display()));
        }
        Ok(())
    }
}

/// Python 解析顺序：手填路径 > web 目录里的虚拟环境 > PATH 上的 python。
/// 本机 `E:/dev/NOS/web/venv` 就是靠第二条命中的。
pub fn resolve_python(work_dir: &Path, configured: &str) -> PathBuf {
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    #[cfg(windows)]
    let candidates: [&str; 4] = [
        ".venv/Scripts/python.exe",
        "venv/Scripts/python.exe",
        "../.venv/Scripts/python.exe",
        "../venv/Scripts/python.exe",
    ];
    #[cfg(not(windows))]
    let candidates: [&str; 4] = [
        ".venv/bin/python",
        "venv/bin/python",
        "../.venv/bin/python",
        "../venv/bin/python",
    ];
    for relative in candidates {
        let path = work_dir.join(relative);
        if path.is_file() { return path; }
    }
    #[cfg(windows)]
    { PathBuf::from("python.exe") }
    #[cfg(not(windows))]
    { PathBuf::from("python3") }
}

/// 「自动探测」：从环境变量、当前目录、可执行文件所在目录向上找 NOS 的 web 目录。
/// 同时要求 `app/daozai.py` 存在，避免命中一份没有岛仔接口的旧版本。
pub fn detect_work_dir() -> Option<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(value) = std::env::var("NOS_WEB_DIR") {
        if !value.trim().is_empty() { bases.push(PathBuf::from(value.trim())); }
    }
    if let Ok(cwd) = std::env::current_dir() { push_bases(&mut bases, cwd); }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() { push_bases(&mut bases, parent.to_path_buf()); }
    }
    for base in bases {
        for relative in ["", "web", "NOS/web", "NOS", "../NOS/web", "../../NOS/web"] {
            let dir = base.join(relative);
            if dir.join("app/main.py").is_file() && dir.join("app/daozai.py").is_file() {
                return Some(dir);
            }
        }
    }
    None
}

fn push_bases(bases: &mut Vec<PathBuf>, root: PathBuf) {
    // 双击 exe 时 cwd 通常在 target/.../debug，往上级走六层才够到 E:/dev 这类仓库根。
    let mut current = root;
    for _ in 0..6 {
        bases.push(current.clone());
        current = current.join("..");
    }
}

// ---------------------------------------------------------------- 状态

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// 端口没在监听。
    #[default]
    Stopped,
    /// 刚发出启动命令，还在等端口就绪。
    Starting,
    /// 端口在监听、岛仔接口可用，且进程由本次岛仔拉起。
    Running,
    /// 端口在监听、接口可用，但进程不是本次岛仔拉起的（外部服务 / 上次会话遗留）。
    External,
    /// 端口在监听，但接口不可用（不是 NOS、令牌不匹配、版本过旧）。
    Unhealthy,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub phase: Phase,
    /// 由岛仔拉起时记录 PID；外部服务时为 PID 文件里能确认的上次 PID。
    pub pid: Option<u32>,
    pub detail: String,
    pub checked_at: i64,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self.phase {
            Phase::Stopped => "未运行",
            Phase::Starting => "启动中",
            Phase::Running => "运行中",
            Phase::External => "外部运行",
            Phase::Unhealthy => "异常",
        }
    }
    pub fn healthy(&self) -> bool { matches!(self.phase, Phase::Running | Phase::External) }
    pub fn starting(&self) -> Status {
        Status {
            phase: Phase::Starting,
            detail: "正在等待 uvicorn 就绪".into(),
            checked_at: chrono::Local::now().timestamp(),
            ..self.clone()
        }
    }
}

/// 端口是否在监听。`0.0.0.0` / `::` 只从回环地址探，避免走真实网卡。
pub fn port_open(host: &str, port: u16) -> bool {
    let host = if host.eq_ignore_ascii_case("0.0.0.0") || host == "::" { DEFAULT_HOST } else { host };
    let address = host.parse::<Ipv4Addr>().ok()
        .map(|ip| SocketAddr::from((ip, port)))
        .or_else(|| (host, port).to_socket_addrs().ok().and_then(|mut it| it.next()))
        .unwrap_or_else(|| SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok()
}

/// 只做「这台服务是不是能给我提供课表」的轻量探测，不拉整份 agenda。
/// 用独立 Agent 且不设代理：本机地址不该被系统代理劫持。
pub fn ping(base: &str, token: &str) -> Result<(), String> {
    let url = format!("{}/api/daozai/ping", base.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout(PING_TIMEOUT).build();
    let request = agent
        .get(&url)
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {}", token.trim()));
    match request.call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err("服务已启动，但访问令牌不匹配".into())
        }
        Err(ureq::Error::Status(503, _)) => {
            Err("服务已启动，但服务端未配置 NOS_API_TOKEN".into())
        }
        Err(ureq::Error::Status(404, _)) => {
            Err("服务已启动，但这份 NOS 没有岛仔联动接口".into())
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("服务返回 HTTP {code}")),
        Err(_) => Err("服务没有响应".into()),
    }
}

/// 探测一次状态。端口没开时不发任何 HTTP 请求，空闲时零流量。
pub fn probe(cfg: &Config, managed_pid: Option<u32>) -> Status {
    let checked_at = chrono::Local::now().timestamp();
    let recorded = read_pid(&cfg.pid_path());
    if !port_open(cfg.host(), cfg.port) {
        return Status {
            phase: Phase::Stopped,
            pid: managed_pid.or_else(|| recorded.as_ref().map(|r| r.pid)),
            detail: format!("{} 未监听到服务", cfg.base_url()),
            checked_at,
        };
    }
    match ping(&cfg.base_url(), &cfg.token) {
        Ok(()) => Status {
            phase: if managed_pid.is_some() { Phase::Running } else { Phase::External },
            pid: managed_pid.or_else(|| recorded.as_ref().map(|r| r.pid)),
            detail: format!("{} 接口正常", cfg.base_url()),
            checked_at,
        },
        Err(error) => Status {
            phase: Phase::Unhealthy,
            pid: managed_pid.or_else(|| recorded.as_ref().map(|r| r.pid)),
            detail: error,
            checked_at,
        },
    }
}

// ---------------------------------------------------------------- PID 记录

#[derive(Clone, Debug, PartialEq)]
pub struct PidRecord {
    pub pid: u32,
    pub base_url: String,
}

pub fn write_pid(path: &Path, pid: u32, base_url: &str) {
    let _ = fs::write(path, format!("pid={pid}\nurl={base_url}\n"));
}

pub fn read_pid(path: &Path) -> Option<PidRecord> {
    let text = fs::read_to_string(path).ok()?;
    let mut pid = None;
    let mut url = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("pid=") { pid = value.trim().parse::<u32>().ok(); }
        if let Some(value) = line.strip_prefix("url=") { url = value.trim().to_string(); }
    }
    Some(PidRecord { pid: pid?, base_url: url })
}

// ---------------------------------------------------------------- 进程

/// 岛仔拉起的一台 NOS Web 服务。丢弃（置 None）即停止进程树。
pub struct Server {
    child: Child,
    pub pid: u32,
    pub log: PathBuf,
    pid_file: PathBuf,
    /// 作业对象句柄必须活到岛仔进程结束；Drop 时关闭它会连带杀掉子进程。
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl Server {
    pub fn alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    pub fn stop(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut kill = Command::new("taskkill.exe");
            kill.args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            kill.creation_flags(0x08000000);
            let _ = kill.status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.pid_file);
    }
}

impl Drop for Server {
    fn drop(&mut self) { self.stop(); }
}

/// 停止一个由岛仔记录过的 PID（用于「上次会话遗留 / 外部运行」的停止按钮）。
/// 只接受 PID 文件里出现过的进程号，不提供任意 PID 清理入口。
pub fn stop_pid(pid: u32) {
    if pid == 0 { return; }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut kill = Command::new("taskkill.exe");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        kill.creation_flags(0x08000000);
        let _ = kill.status();
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 只用于开发机，直接发 SIGKILL。
        let _ = Command::new("kill").args(["-9", &pid.to_string()])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

/// 启动服务。端口已被占用时拒绝重复启动，避免两个 uvicorn 抢同一个端口。
pub fn start(cfg: &Config) -> Result<Server, String> {
    cfg.validate()?;
    if port_open(cfg.host(), cfg.port) {
        return Err(format!(
            "{} 已被占用；若 NOS Web 已在运行，请先停止它或换一个端口",
            cfg.base_url()
        ));
    }
    fs::create_dir_all(&cfg.log_dir).map_err(|e| format!("无法创建日志目录：{e}"))?;
    let log = cfg.log_path();
    if fs::metadata(&log).map_or(false, |m| m.len() > LOG_LIMIT) {
        let previous = cfg.previous_log_path();
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&log, &previous);
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&log)
        .map_err(|e| format!("无法写入日志：{e}"))?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(file, "\n===== 岛仔启动 NOS Web {stamp} · {} =====", cfg.base_url());
    let stdout = file.try_clone().map_err(|e| format!("无法复制日志句柄：{e}"))?;

    let exe = cfg.python_exe();
    let mut command = Command::new(&exe);
    command
        .args(cfg.uvicorn_args())
        .current_dir(cfg.dir())
        .env("NOS_API_TOKEN", cfg.token.trim())
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(file));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不弹黑色控制台窗口；日志已经重定向到文件。
        command.creation_flags(0x08000000);
    }
    let child = command.spawn().map_err(|e| {
        format!("无法启动 {}：{e}；请检查 Python 路径与 requirements.txt 是否已安装", exe.display())
    })?;
    let pid = child.id();
    #[cfg(windows)]
    let job = job::attach(pid);
    let pid_file = cfg.pid_path();
    write_pid(&pid_file, pid, &cfg.base_url());
    #[cfg(windows)]
    { Ok(Server { child, pid, log, pid_file, _job: job }) }
    #[cfg(not(windows))]
    { Ok(Server { child, pid, log, pid_file }) }
}

// ---------------------------------------------------------------- 作业对象（仅 Windows）

/// 把 uvicorn 放进「句柄关闭即杀进程」的作业对象。
/// 托盘退出用 `ExitProcess`，Rust 的 `Drop` 不会跑，靠操作系统收走子进程才不会留下
/// 一个占着 8000 端口的孤儿 Python。不支持嵌套作业的环境下降级为 PID 文件兜底。
#[cfg(windows)]
mod job {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_SET_QUOTA: u32 = 0x0100;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub struct Job(*mut c_void);
    impl Drop for Job {
        fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
    }

    pub fn attach(pid: u32) -> Option<Job> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() { return None; }
            let mut info = ExtendedLimit::default();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &info as *const _ as *const c_void,
                std::mem::size_of::<ExtendedLimit>() as u32,
            ) != 0;
            let process = if ok { OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, 0, pid) } else { std::ptr::null_mut() };
            let assigned = !process.is_null() && AssignProcessToJobObject(handle, process) != 0;
            if !process.is_null() { CloseHandle(process); }
            if assigned { Some(Job(handle)) } else { CloseHandle(handle); None }
        }
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("daozai-nos-server-{tag}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }

    fn fixture(tag: &str) -> (PathBuf, Config) {
        let root = temp(tag);
        let web = root.join("web");
        fs::create_dir_all(web.join("app")).unwrap();
        fs::write(web.join("app/main.py"), "").unwrap();
        fs::write(web.join("app/daozai.py"), "").unwrap();
        let mut cfg = Config::default();
        cfg.work_dir = web.to_string_lossy().into_owned();
        cfg.log_dir = root.join("logs");
        (root, cfg)
    }

    #[test]
    fn default_config_targets_local_loopback_and_uvicorn_entry() {
        let cfg = Config::default();
        assert_eq!(cfg.host(), "127.0.0.1");
        assert_eq!(cfg.port, 8000);
        assert_eq!(cfg.base_url(), "http://127.0.0.1:8000");
        assert_eq!(cfg.uvicorn_args(), vec![
            "-m", "uvicorn", "app.main:app", "--host", "127.0.0.1", "--port", "8000"
        ]);
        let mut ipv6 = cfg.clone();
        ipv6.host = "::1".into();
        assert_eq!(ipv6.base_url(), "http://[::1]:8000");
        let mut blank = cfg;
        blank.host = "  ".into();
        assert_eq!(blank.host(), DEFAULT_HOST, "留空要回退到回环地址");
    }

    #[test]
    fn validation_rejects_missing_web_dir_and_bad_python() {
        let (root, mut cfg) = fixture("validate");
        assert!(cfg.validate().is_ok());
        cfg.work_dir = root.join("missing").to_string_lossy().into_owned();
        assert!(cfg.validate().unwrap_err().contains("目录不存在"));
        cfg.work_dir = root.to_string_lossy().into_owned();
        assert!(cfg.validate().unwrap_err().contains("app/main.py"));
        cfg.work_dir = root.join("web").to_string_lossy().into_owned();
        cfg.port = 0;
        assert!(cfg.validate().unwrap_err().contains("端口"));
        cfg.port = 8000;
        cfg.host = "127.0 0.1".into();
        assert!(cfg.validate().unwrap_err().contains("监听地址"));
        cfg.host = DEFAULT_HOST.into();
        cfg.python = root.join("nope/python.exe").to_string_lossy().into_owned();
        assert!(cfg.validate().unwrap_err().contains("Python 不存在"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn python_resolution_prefers_the_project_venv() {
        let (root, mut cfg) = fixture("python");
        let web = cfg.dir();
        assert!(!cfg.python_exe().is_absolute() || cfg.python_exe().is_file(),
            "没有虚拟环境时回退到 PATH 上的 python");
        #[cfg(windows)]
        let venv = web.join("venv/Scripts/python.exe");
        #[cfg(not(windows))]
        let venv = web.join("venv/bin/python");
        fs::create_dir_all(venv.parent().unwrap()).unwrap();
        fs::write(&venv, "").unwrap();
        assert_eq!(resolve_python(&web, ""), venv);
        cfg.python = root.join("custom.exe").to_string_lossy().into_owned();
        assert_eq!(resolve_python(&web, cfg.python.trim()), PathBuf::from(&cfg.python),
            "手填路径优先级最高");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pid_record_round_trips_and_ignores_garbage() {
        let root = temp("pid");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(PID_FILE);
        assert!(read_pid(&path).is_none(), "没有文件时不应有 PID");
        write_pid(&path, 4321, "http://127.0.0.1:8000");
        assert_eq!(read_pid(&path).unwrap(), PidRecord { pid: 4321, base_url: "http://127.0.0.1:8000".into() });
        fs::write(&path, "nothing useful\n").unwrap();
        assert!(read_pid(&path).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_labels_and_health() {
        let mut status = Status::default();
        assert_eq!(status.phase, Phase::Stopped);
        assert_eq!(status.label(), "未运行");
        assert!(!status.healthy());
        status.phase = Phase::Running;
        assert!(status.healthy());
        assert_eq!(status.starting().phase, Phase::Starting, "启动中保留 PID 等信息");
        assert!(Status { phase: Phase::External, ..Default::default() }.healthy());
        assert!(!Status { phase: Phase::Unhealthy, ..Default::default() }.healthy());
        assert_eq!(Status { phase: Phase::External, ..Default::default() }.label(), "外部运行");
        assert_eq!(Status { phase: Phase::Unhealthy, ..Default::default() }.label(), "异常");
    }

    #[test]
    fn probe_reports_stopped_without_touching_http_when_port_is_closed() {
        // 取一个几乎不可能被占用的高端口；若真被占用则跳过断言 HTTP 部分。
        let (root, mut cfg) = fixture("probe");
        cfg.port = 65_300;
        if !port_open(cfg.host(), cfg.port) {
            let status = probe(&cfg, None);
            assert_eq!(status.phase, Phase::Stopped);
            assert!(status.detail.contains("未监听到服务"));
        }
        fs::remove_dir_all(root).unwrap();
    }
}
