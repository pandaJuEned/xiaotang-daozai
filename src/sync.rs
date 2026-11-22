//! 岛仔多机同步（P1）：设备身份、增量游标、outbox / inbox 与退避。
//!
//! 这一层只负责「把变更搬来搬去」，不懂业务：
//!
//! - `outbox` 是待上传的操作。**变更发生时就要落盘**——托盘退出走 `ExitProcess`，
//!   Rust 的 `Drop` 不执行，不能等同步线程来写。
//! - `inbox` 是已拉到、尚未被业务层应用的记录。**先落盘再推进游标**，
//!   避免「游标前进了、数据却丢了」。P2 起由 `sync_map` 消费 inbox。
//! - `sync-state.json` 只存游标、时钟偏差与最近成功时间；令牌进 Windows 凭据管理器。
//!
//! 协议见 `E:/dev/LDH/岛仔双机同步-方案.md` 与 `NOS/web/README.md`「多机同步（P0）」。

use eframe::egui;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::poll_clock::PollClock;

/// 正常同步间隔（秒）。两台机器的小数据量，15 秒足够。
pub const INTERVAL: i64 = 15;
/// 失败退避上限（秒）。
const MAX_BACKOFF: f64 = 600.0;
/// 首次失败后的退避基数（秒）。
const BACKOFF_BASE: f64 = 30.0;
/// 单次拉取的记录上限（与服务端默认一致）。
const LIMIT: usize = 200;
/// 单批上传上限：服务端硬限制 200，这里留一半，避免一次请求体过大。
const MAX_BATCH: usize = 100;

// ---------------------------------------------------------------- 路径

pub fn device_path() -> PathBuf {
    crate::model::config_dir().join("device.json")
}
pub fn state_path() -> PathBuf {
    crate::model::config_dir().join("sync-state.json")
}
pub fn outbox_path() -> PathBuf {
    crate::model::config_dir().join("sync-outbox.jsonl")
}
pub fn inbox_path() -> PathBuf {
    crate::model::config_dir().join("sync-inbox.jsonl")
}

/// 两处写入（UI 线程追加、同步线程压缩）互斥；跨进程由单实例互斥体兜底。
static OUTBOX_LOCK: Mutex<()> = Mutex::new(());
static INBOX_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------- 设备身份

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceFile {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

fn random_hex(bytes: usize) -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut buffer = vec![0u8; bytes];
    if SystemRandom::new().fill(&mut buffer).is_ok() {
        return buffer.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    // 极端情况下 RNG 不可用：用时间 + pid 兜底，至少保证进程内唯一。
    format!(
        "{:016x}{:08x}",
        chrono::Local::now().timestamp_nanos_opt().unwrap_or_default(),
        std::process::id()
    )
}

/// 生成一个稳定 id（32 位十六进制），供业务记录（待办等）使用。
pub fn random_id() -> String {
    random_hex(16)
}

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(40)
        .collect()
}

/// 读取本机身份；首次调用时生成随机 id 并落盘。
pub fn load_device() -> DeviceFile {
    load_device_at(&device_path())
}

fn load_device_at(path: &Path) -> DeviceFile {
    if let Ok(raw) = std::fs::read(path) {
        if let Ok(mut device) = serde_json::from_slice::<DeviceFile>(&raw) {
            if !device.id.trim().is_empty() {
                device.name = sanitize_name(&device.name);
                return device;
            }
        }
    }
    let fallback = std::env::var("COMPUTERNAME").unwrap_or_default();
    let device = DeviceFile {
        id: random_hex(16),
        name: sanitize_name(&fallback),
    };
    let _ = save_device_at(path, &device);
    device
}

fn save_device_at(path: &Path, device: &DeviceFile) -> Result<(), String> {
    let raw = serde_json::to_vec(device).map_err(|error| format!("设备信息无法序列化：{error}"))?;
    crate::model::atomic_write(path, &raw).map_err(|error| format!("写入设备信息失败：{error}"))
}

/// 修改本机显示名。设备 id 不变，服务端按 id 覆盖名称。
pub fn rename_device(name: &str) -> Result<DeviceFile, String> {
    let mut device = load_device();
    device.name = sanitize_name(name);
    save_device_at(&device_path(), &device)?;
    Ok(device)
}

/// 给界面显示的默认名：没填时用 id 前 8 位，避免出现空白行。
pub fn display_name(device: &DeviceFile) -> String {
    if device.name.trim().is_empty() {
        device.id.chars().take(8).collect()
    } else {
        device.name.clone()
    }
}

// ---------------------------------------------------------------- 配置

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// 服务地址，如 `https://nos.example.com`。
    pub base_url: String,
    /// Bearer 令牌，只进凭据管理器。
    pub token: String,
    /// 稳定设备 id（`device.json`）。
    pub device_id: String,
    /// 显示名（`device.json`）。
    pub device_name: String,
}

impl Config {
    pub fn ready(&self) -> bool {
        !normalize_base(&self.base_url).is_empty()
            && !self.token.trim().is_empty()
            && !self.device_id.trim().is_empty()
    }
}

/// 地址统一成「无尾斜杠、带 scheme」。用户只填 `nos.example.com` 也能用。
pub fn normalize_base(raw: &str) -> String {
    let mut value = raw.trim().trim_end_matches('/').to_string();
    if value.is_empty() {
        return value;
    }
    if !value.starts_with("http://") && !value.starts_with("https://") {
        value = format!("http://{value}");
    }
    value
}

/// 公网明文地址提示：只用于界面提醒，不阻止（局域网 http 是常态）。
pub fn insecure_public(base: &str) -> bool {
    let lower = base.trim().to_ascii_lowercase();
    if !lower.starts_with("http://") {
        return false;
    }
    let host = lower["http://".len()..].split('/').next().unwrap_or("");
    let local = host.starts_with("127.")
        || host.starts_with("localhost")
        || host.starts_with("[::1]")
        || host.starts_with("192.168.")
        || host.starts_with("10.")
        || (host.starts_with("172.")
            && host
                .split('.')
                .nth(1)
                .and_then(|part| part.parse::<u8>().ok())
                .is_some_and(|second| (16..=31).contains(&second)));
    !local
}

// ---------------------------------------------------------------- 凭据

#[cfg(windows)]
pub mod secrets {
    /// 测试用独立条目：`delete()` 会真的删凭据，不能和正式条目同名。
    const SERVICE: &str = if cfg!(test) { "Daozai.Sync.test" } else { "Daozai.Sync" };

    pub fn save(token: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, "token")
            .map_err(|error| format!("凭据管理器不可用：{error}"))?;
        entry
            .set_password(token)
            .map_err(|error| format!("保存令牌失败：{error}"))?;
        // 课表与多端同步共用一枚 NOS PAT：同步页保存时一并写入课表条目，
        // 这样旧版课表页也能直接读到同一枚令牌。
        crate::nos::secrets::save(token)
    }

    pub fn load() -> Result<String, String> {
        let entry = keyring::Entry::new(SERVICE, "token")
            .map_err(|error| format!("凭据管理器不可用：{error}"))?;
        if let Ok(token) = entry.get_password() {
            if !token.trim().is_empty() {
                return Ok(token);
            }
        }
        // 合并页只填一次令牌：同步条目没有时回退到课表条目。
        crate::nos::secrets::load()
    }

    pub fn delete() {
        if let Ok(entry) = keyring::Entry::new(SERVICE, "token") {
            let _ = entry.delete_credential();
        }
        crate::nos::secrets::delete();
    }
}

#[cfg(not(windows))]
pub mod secrets {
    pub fn save(_token: &str) -> Result<(), String> {
        Err("仅 Windows 支持凭据管理器".into())
    }
    pub fn load() -> Result<String, String> {
        Err("仅 Windows 支持凭据管理器".into())
    }
    pub fn delete() {}
}

// ---------------------------------------------------------------- 游标

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StateFile {
    #[serde(default)]
    pub cursor: i64,
    /// 最近一次成功同步的本机时间（Unix 秒）。
    #[serde(default)]
    pub last_ok: i64,
    /// 服务端时间 - 本机时间（毫秒）；P4 的番茄倒计时会用到。
    #[serde(default)]
    pub skew_ms: i64,
}

impl StateFile {
    pub fn load() -> Self {
        load_state_at(&state_path())
    }

    pub fn save(&self) {
        let _ = save_state_at(&state_path(), self);
    }
}

fn load_state_at(path: &Path) -> StateFile {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<StateFile>(&raw).ok())
        .unwrap_or_default()
}

fn save_state_at(path: &Path, state: &StateFile) -> Result<(), String> {
    let raw = serde_json::to_vec(state).map_err(|error| format!("同步状态无法序列化：{error}"))?;
    crate::model::atomic_write(path, &raw).map_err(|error| format!("写入同步状态失败：{error}"))
}

// ---------------------------------------------------------------- 记录与操作

/// 服务端返回的一条记录。`data` 保持原样，由 `sync_map` 解释。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Record {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub rev: i64,
    #[serde(default)]
    pub seq: i64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// 待上传的一次变更。字段与服务端 `OpIn` 对齐。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Op {
    pub op_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub base_rev: i64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub client_at: i64,
}

impl Op {
    pub fn new(kind: &str, id: &str, base_rev: i64, day: &str, data: serde_json::Value) -> Self {
        Self {
            op_id: random_hex(16),
            kind: kind.into(),
            id: id.into(),
            base_rev,
            deleted: false,
            day: day.into(),
            data,
            client_at: chrono::Local::now().timestamp(),
        }
    }
}

// ---------------------------------------------------------------- outbox

/// 追加一条待上传操作。**变更发生时立即调用**，不要攒到同步线程。
pub fn enqueue(op: &Op) -> Result<(), String> {
    enqueue_at(&outbox_path(), op)
}

fn enqueue_at(path: &Path, op: &Op) -> Result<(), String> {
    let _guard = OUTBOX_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建同步目录失败：{error}"))?;
    }
    let mut line = serde_json::to_vec(op).map_err(|error| format!("操作无法序列化：{error}"))?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("打开 outbox 失败：{error}"))?;
    file.write_all(&line)
        .and_then(|_| file.sync_data())
        .map_err(|error| format!("写入 outbox 失败：{error}"))
}

pub fn pending_ops() -> Vec<Op> {
    pending_at(&outbox_path())
}

fn pending_at(path: &Path) -> Vec<Op> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Op>(line).ok())
        .collect()
}

/// 确认已处理的操作：只保留未确认的，原子重写整个文件。
pub fn ack_ops(acked: &HashSet<String>) {
    ack_at(&outbox_path(), acked)
}

fn ack_at(path: &Path, acked: &HashSet<String>) {
    if acked.is_empty() {
        return;
    }
    let _guard = OUTBOX_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let remaining: Vec<Op> = pending_at(path)
        .into_iter()
        .filter(|op| !acked.contains(&op.op_id))
        .collect();
    let mut body = Vec::new();
    for op in remaining {
        if let Ok(mut line) = serde_json::to_vec(&op) {
            line.push(b'\n');
            body.extend_from_slice(&line);
        }
    }
    let _ = crate::model::atomic_write(path, &body);
}

// ---------------------------------------------------------------- inbox

/// 先落盘、再推进游标；P2 的 `sync_map` 从这里取数据。
pub fn append_inbox(records: &[Record]) -> Result<(), String> {
    append_inbox_at(&inbox_path(), records)
}

fn append_inbox_at(path: &Path, records: &[Record]) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }
    let _guard = INBOX_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建同步目录失败：{error}"))?;
    }
    let mut body = Vec::new();
    for record in records {
        body.extend_from_slice(
            &serde_json::to_vec(record).map_err(|error| format!("记录无法序列化：{error}"))?,
        );
        body.push(b'\n');
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("打开 inbox 失败：{error}"))?;
    file.write_all(&body)
        .and_then(|_| file.sync_data())
        .map_err(|error| format!("写入 inbox 失败：{error}"))
}

pub fn inbox_len() -> usize {
    std::fs::read_to_string(inbox_path())
        .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// 取出 inbox 里的全部记录（不清空）。调用方应用成功后用 `replace_inbox` 写回剩余。
pub fn read_inbox() -> Vec<Record> {
    let _guard = INBOX_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let Ok(raw) = std::fs::read_to_string(inbox_path()) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Record>(line).ok())
        .collect()
}

/// 用剩余记录重写 inbox。P2 只消费 `todo_item`，其它类型留给后续阶段。
pub fn replace_inbox(records: &[Record]) {
    let _guard = INBOX_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut body = Vec::new();
    for record in records {
        if let Ok(mut line) = serde_json::to_vec(record) {
            line.push(b'\n');
            body.extend_from_slice(&line);
        }
    }
    let _ = crate::model::atomic_write(&inbox_path(), &body);
}

pub fn clear_inbox() {
    let _guard = INBOX_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _ = crate::model::atomic_write(&inbox_path(), b"");
}

// ---------------------------------------------------------------- 回执

/// 一条操作的处理结果。UI 线程靠它把 `rev` 与 `pending` 对齐。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Ack {
    pub op_id: String,
    pub status: String,
    #[serde(default)]
    pub rev: Option<i64>,
    #[serde(default)]
    pub seq: Option<i64>,
}

static ACK_LOCK: Mutex<()> = Mutex::new(());

fn append_ack(ack: &Ack) {
    let _guard = ACK_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let path = crate::model::config_dir().join("sync-acks.jsonl");
    let Ok(mut line) = serde_json::to_vec(ack) else {
        return;
    };
    line.push(b'\n');
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(&line).and_then(|_| file.sync_data()));
}

/// 取走全部回执并清空。UI 每 500ms 泵一次，正常不会堆积。
pub fn drain_acks() -> Vec<Ack> {
    let _guard = ACK_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let path = crate::model::config_dir().join("sync-acks.jsonl");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let acks = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Ack>(line).ok())
        .collect();
    let _ = crate::model::atomic_write(&path, b"");
    acks
}

// ---------------------------------------------------------------- HTTP

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct UserInfo {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Device {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub last_seen_at: String,
    /// 最后一次 presence：各端天气、系统监控等短状态。
    #[serde(default)]
    pub presence: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Hello {
    #[serde(default)]
    pub server_now: i64,
    #[serde(default)]
    pub cursor: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub user: UserInfo,
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Changes {
    #[serde(default)]
    pub cursor: i64,
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub server_now: i64,
    #[serde(default)]
    pub records: Vec<Record>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DevicesResponse {
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Serialize)]
struct OpsRequest<'a> {
    device: &'a str,
    ops: &'a [Op],
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct OpResult {
    #[serde(default)]
    pub op_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub rev: Option<i64>,
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub detail: Option<String>,
    /// `conflict` 时服务端的整条现状；客户端据此采纳并落冲突箱。
    #[serde(default)]
    pub record: Option<Record>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct OpsResponse {
    #[serde(default)]
    pub results: Vec<OpResult>,
    #[serde(default)]
    pub cursor: i64,
    #[serde(default)]
    pub server_now: i64,
}

fn http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(401, _) => "令牌无效或已被撤销，请在设置里重新生成".into(),
        ureq::Error::Status(403, _) => "令牌缺少同步权限（需要 sync:read / sync:write）".into(),
        ureq::Error::Status(404, _) => "服务端没有同步接口，请升级 NOS".into(),
        ureq::Error::Status(413, _) => "单次提交的数据过大".into(),
        ureq::Error::Status(503, _) => "服务端未配置访问令牌".into(),
        ureq::Error::Status(code, _) => format!("服务返回 {code}"),
        _ => "网络不可用".into(),
    }
}

fn read_json<T: DeserializeOwned>(response: ureq::Response) -> Result<T, String> {
    let mut body = String::new();
    response
        .into_reader()
        .take(4 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|error| format!("读取响应失败：{error}"))?;
    serde_json::from_str(&body).map_err(|error| format!("服务返回的数据无法解析：{error}"))
}

fn post_json<T: DeserializeOwned>(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    body: &impl Serialize,
) -> Result<T, String> {
    let payload = serde_json::to_vec(body).map_err(|error| format!("请求无法序列化：{error}"))?;
    let response = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_bytes(&payload)
        .map_err(http_error)?;
    read_json(response)
}

fn get_json<T: DeserializeOwned>(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
) -> Result<T, String> {
    let response = agent
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(http_error)?;
    read_json(response)
}

fn hello(agent: &ureq::Agent, cfg: &Config) -> Result<Hello, String> {
    let base = normalize_base(&cfg.base_url);
    let body = serde_json::json!({
        "device": {
            "id": cfg.device_id,
            "name": cfg.device_name,
            "platform": "windows",
            "app": env!("CARGO_PKG_VERSION"),
        }
    });
    post_json(agent, &format!("{base}/api/daozai/hello"), &cfg.token, &body)
}

fn changes(agent: &ureq::Agent, cfg: &Config, since: i64) -> Result<Changes, String> {
    let base = normalize_base(&cfg.base_url);
    get_json(
        agent,
        &format!("{base}/api/daozai/changes?since={since}&limit={LIMIT}"),
        &cfg.token,
    )
}

fn devices(agent: &ureq::Agent, cfg: &Config) -> Result<DevicesResponse, String> {
    let base = normalize_base(&cfg.base_url);
    get_json(agent, &format!("{base}/api/daozai/devices"), &cfg.token)
}

/// 在场心跳：不进变更流、90 秒过期，只给其他设备「看一眼」本机天气与监控。
fn send_presence(agent: &ureq::Agent, cfg: &Config, state: &PresenceOut) -> Result<(), String> {
    let base = normalize_base(&cfg.base_url);
    let body = serde_json::json!({
        "device": cfg.device_id,
        "state": state,
    });
    let _: serde_json::Value = post_json(
        agent,
        &format!("{base}/api/daozai/presence"),
        &cfg.token,
        &body,
    )?;
    Ok(())
}

fn submit(agent: &ureq::Agent, cfg: &Config, ops: &[Op]) -> Result<OpsResponse, String> {
    let base = normalize_base(&cfg.base_url);
    post_json(
        agent,
        &format!("{base}/api/daozai/ops"),
        &cfg.token,
        &OpsRequest {
            device: &cfg.device_id,
            ops,
        },
    )
}

/// 测试连接：只读设备列表，不注册临时设备。
pub fn test_connection(base_url: &str, token: &str) -> Result<String, String> {
    let base = normalize_base(base_url);
    if base.is_empty() {
        return Err("请先填写服务器地址".into());
    }
    if token.trim().is_empty() {
        return Err("请先填写访问令牌".into());
    }
    let agent = crate::nos::http_agent();
    let response: DevicesResponse = get_json(
        &agent,
        &format!("{base}/api/daozai/devices"),
        token.trim(),
    )?;
    Ok(format!(
        "连接成功 · 服务器上已有 {} 台设备",
        response.devices.len()
    ))
}

// ---------------------------------------------------------------- 快照

/// 本机每 30 秒上报一次的短状态：只放天气与监控这类「看一眼」的数据。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceOut {
    #[serde(default)]
    pub weather: Option<PresenceWeather>,
    #[serde(default)]
    pub monitor: Option<PresenceMonitor>,
    /// 待提醒数量：只报数字，不报内容——「哪里有通知」而不是「通知了什么」。
    #[serde(default)]
    pub alerts: u32,
    /// 本机番茄状态：专注/休息与剩余分钟，实时在场的一部分。
    #[serde(default)]
    pub focus: Option<PresenceFocus>,
    /// 本机电池：没有电池的台式机为 None 或 percent None。
    #[serde(default)]
    pub battery: Option<PresenceBattery>,
    /// 未读消息摘要（来源 + 数量，不含内容）；已读后不再上报，其他端自然消失。
    #[serde(default)]
    pub messages: Vec<PresenceMessage>,
    /// 正在播放的媒体来源与曲目，方便其他设备知道谁在用哪台机器听什么。
    #[serde(default)]
    pub music: Option<PresenceMusic>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceMessage {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub count: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceMusic {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub playing: bool,
    /// 已播秒数：其他设备据此显示「到哪了」，接力时也从这里继续。
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub duration: f64,
    /// 只有 WebDAV 曲目才有可续播地址；系统播放器的曲目只能展示。
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceBattery {
    #[serde(default)]
    pub percent: Option<u8>,
    #[serde(default)]
    pub charging: bool,
    /// 台式机没有电池：ac_online 时显示「电源供电」。
    #[serde(default)]
    pub ac_online: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceFocus {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub remaining: u32,
    #[serde(default)]
    pub running: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceWeather {
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub temp: f32,
    #[serde(default)]
    pub code: u8,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PresenceMonitor {
    #[serde(default)]
    pub cpu: Option<f32>,
    #[serde(default)]
    pub mem: Option<f32>,
    #[serde(default)]
    pub disk: Option<f32>,
    #[serde(default)]
    pub temp: Option<f32>,
    #[serde(default)]
    pub gpu: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub platform: String,
    pub last_seen: String,
    /// 原始 presence JSON；用 `presence_data()` 转成结构化快照。
    pub presence: serde_json::Value,
}

impl DeviceView {
    pub fn presence_data(&self) -> Option<PresenceOut> {
        serde_json::from_value(self.presence.clone()).ok()
    }

    pub fn weather(&self) -> Option<PresenceWeather> {
        self.presence_data().and_then(|presence| presence.weather)
    }

    pub fn monitor(&self) -> Option<PresenceMonitor> {
        self.presence_data().and_then(|presence| presence.monitor)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub configured: bool,
    pub connected: bool,
    /// 首次增量拉取已完成。未 ready 时 `sync_map` 不上传本地数据，
    /// 避免第二台设备在还没看到服务端记录前就把本地条目重复推上去。
    pub ready: bool,
    pub user: String,
    pub cursor: i64,
    pub pending: usize,
    pub devices: Vec<DeviceView>,
    pub last_ok: i64,
    pub error: Option<String>,
    pub skew_ms: i64,
    pub last_batch: usize,
    pub inbox: usize,
    /// 当前其它设备正在播的曲目（服务端仲裁结果）；没人播时为 None。
    pub music_remote: Option<RemoteMusic>,
    /// 服务端的播放权版本号：比本机大就说明别人接手了。
    pub music_epoch: i64,
    /// 本机被其它设备接管，Shell 取到后立刻暂停本地播放并清掉这个标记。
    pub music_lost: bool,
}

/// 服务端返回的「谁在播」。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RemoteMusic {
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub updated_at: i64,
}

impl Snapshot {
    pub fn device_name(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|device| device.id == id)
            .map(|device| device.name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| id.chars().take(8).collect())
    }
}

fn device_views(list: Vec<Device>) -> Vec<DeviceView> {
    list.into_iter()
        .map(|device| DeviceView {
            name: if device.name.trim().is_empty() {
                device.id.chars().take(8).collect()
            } else {
                device.name
            },
            id: device.id,
            online: device.online,
            platform: device.platform,
            last_seen: device.last_seen_at,
            presence: device.presence,
        })
        .collect()
}

// ---------------------------------------------------------------- 服务

pub struct Service {
    rx: Receiver<Snapshot>,
    stop: Arc<AtomicBool>,
    /// 请求抢占播放权：UI 点「在手边设备上继续」时置位，同步线程下一轮发出。
    claim: Arc<AtomicBool>,
}

impl Service {
    pub fn start(
        ctx: egui::Context,
        cfg: Config,
        initial: Snapshot,
        presence_state: Arc<Mutex<PresenceOut>>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let claim = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let ask = Arc::clone(&claim);
        let _ = crate::resources::SYNC.spawn(move || {
            run(ctx, cfg, initial, tx, flag, presence_state, ask)
        });
        Self { rx, stop, claim }
    }

    /// 请求在当前设备接手播放（其它设备会在下一轮轮询时暂停）。
    pub fn request_claim(&self) {
        self.claim.store(true, Ordering::Relaxed);
    }

    pub fn try_recv(&self) -> Option<Snapshot> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn backoff_secs(failures: u32) -> i64 {
    // 30 / 60 / 120 / 240 / 480 / 600（上限）
    (BACKOFF_BASE * 2f64.powi(failures.min(6) as i32 - 1)).min(MAX_BACKOFF) as i64
}

fn wait(stop: &AtomicBool, seconds: i64) {
    let deadline = Instant::now() + Duration::from_secs(seconds.max(1) as u64);
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 令牌类错误要重新 hello：令牌可能被撤销，或刚换了 scope。
fn needs_rehello(error: &str) -> bool {
    error.starts_with("令牌")
}

fn log_conflict(result: &OpResult, op: Option<&Op>) {
    let path = crate::model::config_dir().join(format!(
        "sync-conflicts-{}.txt",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    let line = serde_json::json!({
        "at": chrono::Local::now().to_rfc3339(),
        "op": op,
        "status": result.status,
        "rev": result.rev,
        "server": result.record,
    });
    let Ok(mut body) = serde_json::to_vec(&line) else {
        return;
    };
    body.push(b'\n');
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(&body));
}

/// 后台线程：hello → 上传 outbox → 拉 changes → 落 inbox → 推进游标。
fn run(
    ctx: egui::Context,
    cfg: Config,
    mut snap: Snapshot,
    tx: SyncSender<Snapshot>,
    stop: Arc<AtomicBool>,
    presence_state: Arc<Mutex<PresenceOut>>,
    claim_request: Arc<AtomicBool>,
) {
    let agent = crate::nos::http_agent();
    let mut music = MusicClock::default();
    let mut state = StateFile::load();
    let mut clock = PollClock::default();
    let mut failures: u32 = 0;
    let mut hello_ok = false;
    let mut next_devices = 0i64;
    let mut next_presence = 0i64;
    // 每次重建服务都先拉一轮再允许 UI 上传，避免第二台设备抢先推本地数据。
    snap.ready = false;

    while !stop.load(Ordering::Relaxed) {
        if !clock.ready(Duration::from_secs(1)) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        clock = PollClock::now();

        if !cfg.ready() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        snap.configured = true;
        snap.pending = pending_ops().len();
        snap.inbox = inbox_len();
        let mut cycle_ok = true;

        if !hello_ok {
            match hello(&agent, &cfg) {
                Ok(info) => {
                    hello_ok = true;
                    failures = 0;
                    snap.connected = true;
                    snap.error = None;
                    snap.user = if info.user.display_name.trim().is_empty() {
                        info.user.username.clone()
                    } else {
                        info.user.display_name.clone()
                    };
                    snap.devices = device_views(info.devices);
                    snap.cursor = state.cursor;
                    snap.skew_ms = (info.server_now - chrono::Local::now().timestamp()) * 1000;
                    snap.last_ok = chrono::Local::now().timestamp();
                    state.skew_ms = snap.skew_ms;
                    state.save();
                    next_devices = snap.last_ok + 60;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    snap.connected = false;
                    snap.error = Some(error);
                    cycle_ok = false;
                }
            }
        }

        if hello_ok && cycle_ok {
            // 1) 上传：幂等由服务端 op_id 保证；冲突也视为已处理（写冲突箱）。
            let pending = pending_ops();
            if !pending.is_empty() {
                let batch: Vec<Op> = pending.into_iter().take(MAX_BATCH).collect();
                match submit(&agent, &cfg, &batch) {
                    Ok(response) => {
                        let mut acked = HashSet::new();
                        for result in &response.results {
                            append_ack(&Ack {
                                op_id: result.op_id.clone(),
                                status: result.status.clone(),
                                rev: result.rev,
                                seq: result.seq,
                            });
                            match result.status.as_str() {
                                "applied" | "duplicate" => {
                                    acked.insert(result.op_id.clone());
                                }
                                "conflict" => {
                                    acked.insert(result.op_id.clone());
                                    let op = batch.iter().find(|op| op.op_id == result.op_id);
                                    log_conflict(result, op);
                                    // 服务端胜：把它的整条现状塞回 inbox，由 UI 采纳。
                                    match &result.record {
                                        Some(record) => {
                                            let _ = append_inbox(std::slice::from_ref(record));
                                        }
                                        None => {
                                            // 记录在服务端已不存在：合成一条删除，避免本地复活。
                                            if let Some(op) = op {
                                                let _ = append_inbox(&[Record {
                                                    id: op.id.clone(),
                                                    kind: op.kind.clone(),
                                                    deleted: true,
                                                    rev: result.rev.unwrap_or(0),
                                                    ..Default::default()
                                                }]);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        ack_ops(&acked);
                        snap.pending = pending_ops().len();
                    }
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        if needs_rehello(&error) {
                            hello_ok = false;
                            snap.ready = false;
                        }
                        snap.error = Some(error);
                        cycle_ok = false;
                    }
                }
            }
        }

        if hello_ok && cycle_ok {
            // 2) 拉取：必须先落 inbox 再推进游标，否则断电会丢一批记录。
            match changes(&agent, &cfg, state.cursor) {
                Ok(batch) => {
                    if batch.reset {
                        clear_inbox();
                        state.cursor = 0;
                        snap.ready = false;
                        snap.error = Some("服务器游标超前（可能恢复过备份），已重置等待全量重拉".into());
                    } else {
                        if !batch.records.is_empty() {
                            match append_inbox(&batch.records) {
                                Ok(()) => {
                                    state.cursor = batch.cursor;
                                }
                                Err(error) => {
                                    failures = failures.saturating_add(1);
                                    snap.error = Some(error);
                                    snap.ready = false;
                                    cycle_ok = false;
                                }
                            }
                        }
                        if cycle_ok {
                            snap.error = None;
                            snap.ready = true;
                        }
                    }
                    snap.last_batch = batch.records.len();
                    snap.cursor = state.cursor;
                    snap.inbox = inbox_len();
                    state.last_ok = chrono::Local::now().timestamp();
                    state.save();
                    snap.last_ok = state.last_ok;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if needs_rehello(&error) {
                        hello_ok = false;
                    }
                    snap.ready = false;
                    snap.error = Some(error);
                    cycle_ok = false;
                }
            }
        }

        // 3) 设备列表每 60 秒刷新一次；失败不改变主链路的连接状态。
        if hello_ok && chrono::Local::now().timestamp() >= next_devices {
            if let Ok(response) = devices(&agent, &cfg) {
                snap.devices = device_views(response.devices);
            }
            next_devices = chrono::Local::now().timestamp() + 60;
        }

        // 4) 在场心跳：天气与系统监控只走 presence，不进变更流。
        if hello_ok && snap.connected {
            let now = chrono::Local::now().timestamp();
            if now >= next_presence {
                let state = presence_state
                    .lock()
                    .map(|state| state.clone())
                    .unwrap_or_default();
                match send_presence(&agent, &cfg, &state) {
                    Ok(()) => next_presence = now + 30,
                    // 权限不足或网络抖动不报错到主状态，15 秒后重试。
                    Err(_) => next_presence = now + 15,
                }
            }
        }

        // 5) 跨设备播放权：本机在放或别人在放时 2 秒一轮，空闲 5 秒一轮
        //    （空闲也要轮，否则第二台设备要等十几秒才看见有人在播）。
        let music_busy = {
            let state = presence_state
                .lock()
                .map(|state| state.clone())
                .unwrap_or_default();
            state.music.as_ref().is_some_and(|music| music.playing)
                || snap.music_remote.is_some()
                || claim_request.load(Ordering::Relaxed)
        };
        if hello_ok && snap.connected {
            music_tick(
                &agent,
                &cfg,
                &presence_state,
                &mut music,
                &mut snap,
                &claim_request,
            );
        }

        if cycle_ok {
            failures = 0;
            snap.connected = true;
        }
        let _ = tx.try_send(snap.clone());
        ctx.request_repaint();

        // 有同步就保持 5 秒一轮：播放状态靠这轮询才能被别人看见；
        // 正在出声或有人接手时收到 2 秒，让「接手即停」跟手。
        let interval = if music_busy { 2 } else { 5 };
        let wait_secs = if failures == 0 {
            interval
        } else {
            backoff_secs(failures)
        };
        wait(&stop, wait_secs);
    }
}

// ---------------------------------------------------------------- 跨设备播放权

/// 本机播放权的本地视图：服务端 epoch 是唯一事实源。
#[derive(Default)]
struct MusicClock {
    epoch: i64,
    /// 本机当前持有播放权。
    owned: bool,
    /// 已请求认领，等下一轮发出去。
    claim_pending: bool,
    /// 被别的设备接手过：在本机停下再重新开始之前不再自动抢回来，
    /// 否则两台机器会互相抢、来回切歌。
    stood_down: bool,
    /// 上一轮本机是否在出声，用来识别「重新开始播放」这个动作。
    was_playing: bool,
}

enum MusicErr {
    /// 服务端 409：别人接手了，带回服务端 epoch。
    Taken(i64),
    Other,
}

fn music_post<T: DeserializeOwned>(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    body: &impl Serialize,
) -> Result<T, MusicErr> {
    let payload = serde_json::to_vec(body).map_err(|_| MusicErr::Other)?;
    let request = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json");
    match request.send_bytes(&payload) {
        Ok(response) => read_json(response).map_err(|_| MusicErr::Other),
        Err(ureq::Error::Status(409, response)) => {
            let mut raw = String::new();
            let _ = response.into_reader().take(8 * 1024).read_to_string(&mut raw);
            let epoch = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .map(|value| {
                    value["detail"]["epoch"]
                        .as_i64()
                        .or_else(|| value["epoch"].as_i64())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            Err(MusicErr::Taken(epoch))
        }
        Err(_) => Err(MusicErr::Other),
    }
}

/// 解析服务端的 `/music` 响应：没有 `now`（没人播或已过期）时返回 None。
fn parse_remote(value: &serde_json::Value) -> Option<RemoteMusic> {
    let now = value["now"].as_object()?;
    let text = |key: &str| {
        now.get(key)
            .and_then(|item| item.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(RemoteMusic {
        device: text("device"),
        device_name: text("device_name"),
        title: text("title"),
        artist: text("artist"),
        source: text("source"),
        url: text("url"),
        position: now.get("position").and_then(|v| v.as_f64()).unwrap_or(0.0),
        duration: now.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
        updated_at: now.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

fn music_body(cfg: &Config, music: &PresenceMusic, epoch: i64) -> serde_json::Value {    serde_json::json!({
        "device": cfg.device_id,
        "name": cfg.device_name,
        "title": music.title,
        "artist": music.artist,
        "source": music.source,
        "url": music.url,
        "position": music.position,
        "duration": music.duration,
        "playing": music.playing,
        "epoch": epoch,
    })
}

/// 每轮同步做一次播放权维护。三条规则：
/// 1. 本机开始出声但还没拿权 → 自动 claim（别的设备随即暂停）；
/// 2. 持权时持续上报进度；收到 409 说明被接手，交给 Shell 暂停本机；
/// 3. 停止出声时 release，网页和其他设备不再显示在播。
fn music_tick(
    agent: &ureq::Agent,
    cfg: &Config,
    presence: &Arc<Mutex<PresenceOut>>,
    clock: &mut MusicClock,
    snap: &mut Snapshot,
    claim_request: &AtomicBool,
) {
    let base = normalize_base(&cfg.base_url);
    // 每轮重新判定：Shell 处理完暂停后，本机不再出声，下一轮自然不会再置位。
    snap.music_lost = false;
    let music = presence
        .lock()
        .ok()
        .and_then(|state| state.music.clone())
        .filter(|music| !music.title.trim().is_empty() && !music.source.is_empty())
        .unwrap_or_default();
    let playing = music.playing;

    if claim_request.swap(false, Ordering::Relaxed) {
        clock.claim_pending = true;
        clock.stood_down = false;
    }
    // 从「没在放」变成「在放」= 用户明确要在这台机器上听：解除退让并重新拿权。
    if playing && !clock.was_playing {
        clock.stood_down = false;
    }
    clock.was_playing = playing;
    // 被接管过就别自动抢回来，否则两台机器会来回抢。
    if playing && !clock.owned && !clock.claim_pending && !clock.stood_down {
        clock.claim_pending = true;
    }

    if clock.claim_pending {
        match music_post::<serde_json::Value>(
            agent,
            &format!("{base}/api/daozai/music/claim"),
            &cfg.token,
            &music_body(cfg, &music, 0),
        ) {
            Ok(value) => {
                clock.epoch = value["epoch"].as_i64().unwrap_or(clock.epoch);
                clock.owned = true;
                clock.claim_pending = false;
                snap.music_lost = false;
            }
            // 拿不到权就先不上报，避免用旧 epoch 反被误判。
            Err(_) => {}
        }
    } else if clock.owned && playing {
        match music_post::<serde_json::Value>(
            agent,
            &format!("{base}/api/daozai/music/state"),
            &cfg.token,
            &music_body(cfg, &music, clock.epoch),
        ) {
            Ok(_) => {}
            Err(MusicErr::Taken(epoch)) => {
                clock.epoch = epoch.max(clock.epoch);
                clock.owned = false;
                clock.stood_down = true;
                snap.music_lost = true;
            }
            Err(MusicErr::Other) => {}
        }
    } else if clock.owned && !playing {
        let _ = music_post::<serde_json::Value>(
            agent,
            &format!("{base}/api/daozai/music/release"),
            &cfg.token,
            &music_body(cfg, &music, clock.epoch),
        );
        clock.owned = false;
    }

    if let Ok(value) = get_json::<serde_json::Value>(
        agent,
        &format!("{base}/api/daozai/music"),
        &cfg.token,
    ) {
        let epoch = value["epoch"].as_i64().unwrap_or(0);
        snap.music_epoch = epoch;
        if let Some(remote) = parse_remote(&value) {
            if remote.device == cfg.device_id {
                snap.music_remote = None;
            } else {
                snap.music_remote = Some(remote);
            }
        } else {
            snap.music_remote = None;
        }
        // 别人 epoch 更大且本机还在放：立刻让出播放权。
        if playing && clock.owned && epoch > clock.epoch {
            clock.epoch = epoch;
            clock.owned = false;
            clock.stood_down = true;
            snap.music_lost = true;
        }
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "daozai-sync-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("file.jsonl")
    }

    #[test]
    fn normalize_base_adds_scheme_and_strips_trailing_slash() {
        assert_eq!(normalize_base("nos.example.com/"), "http://nos.example.com");
        assert_eq!(normalize_base(" https://a.b/c/ "), "https://a.b/c");
        assert_eq!(normalize_base(""), "");
    }

    #[test]
    fn insecure_public_only_flags_public_plain_http() {
        assert!(insecure_public("http://nos.example.com"));
        assert!(!insecure_public("https://nos.example.com"));
        assert!(!insecure_public("http://127.0.0.1:8000"));
        assert!(!insecure_public("http://192.168.3.136:8000"));
        assert!(!insecure_public("http://172.20.1.2"));
        assert!(insecure_public("http://172.32.1.2"));
    }

    #[test]
    fn outbox_append_ack_and_survives_broken_lines() {
        let path = temp("outbox");
        let _ = std::fs::remove_file(&path);
        let first = Op::new("water_sip", "w1", 0, "2026-11-18", serde_json::json!({"ml":250}));
        let second = Op::new("todo_item", "t1", 2, "", serde_json::json!({"text":"x"}));
        enqueue_at(&path, &first).unwrap();
        enqueue_at(&path, &second).unwrap();
        assert_eq!(pending_at(&path).len(), 2);

        let mut acked = HashSet::new();
        acked.insert(first.op_id.clone());
        ack_at(&path, &acked);
        let left = pending_at(&path);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].op_id, second.op_id);
    }

    #[test]
    fn state_roundtrip_keeps_cursor_and_skew() {
        let path = temp("state");
        let _ = std::fs::remove_file(&path);
        let state = StateFile { cursor: 42, last_ok: 100, skew_ms: -1500 };
        save_state_at(&path, &state).unwrap();
        assert_eq!(load_state_at(&path), state);
    }

    #[test]
    fn device_identity_is_stable_and_name_is_sanitized() {
        let path = temp("device");
        let _ = std::fs::remove_file(&path);
        let first = load_device_at(&path);
        let second = load_device_at(&path);
        assert_eq!(first.id, second.id, "同一文件必须得到同一 id");
        assert!(!first.id.is_empty());
        assert_eq!(sanitize_name("  书房\n台式  "), "书房台式");
        assert_eq!(sanitize_name("一二三四五六七八九十").chars().count(), 10);
    }

    #[test]
    fn hello_changes_and_ops_payloads_parse() {
        let hello: Hello = serde_json::from_str(
            r#"{"server_now":1795,"cursor":7,"scopes":["sync:read"],
                "user":{"username":"a","display_name":"姚"},
                "devices":[{"id":"d1","name":"书房","online":true}]}"#,
        )
        .unwrap();
        assert_eq!(hello.cursor, 7);
        assert_eq!(hello.devices[0].name, "书房");

        let changes: Changes = serde_json::from_str(
            r#"{"cursor":9,"reset":false,"records":[
                {"id":"w1","type":"water_sip","seq":8,"rev":1,"data":{"ml":250}}]}"#,
        )
        .unwrap();
        assert_eq!(changes.records[0].kind, "water_sip");
        assert_eq!(changes.records[0].data["ml"], 250);

        let ops: OpsResponse = serde_json::from_str(
            r#"{"cursor":9,"results":[
                {"op_id":"o1","status":"applied","rev":1,"seq":9},
                {"op_id":"o2","status":"conflict","rev":2,"record":{"id":"t1"}}]}"#,
        )
        .unwrap();
        assert_eq!(ops.results[1].status, "conflict");
        assert_eq!(ops.results[1].rev, Some(2));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_secs(1), 30);
        assert_eq!(backoff_secs(2), 60);
        assert_eq!(backoff_secs(3), 120);
        assert_eq!(backoff_secs(9), 600);
    }

    #[test]
    fn snapshot_resolves_device_name_with_fallback() {
        let snap = Snapshot {
            devices: vec![DeviceView {
                id: "abcdef123456".into(),
                name: "书房台式".into(),
                online: true,
                platform: "windows".into(),
                last_seen: String::new(),
                presence: serde_json::Value::Null,
            }],
            ..Default::default()
        };
        assert_eq!(snap.device_name("abcdef123456"), "书房台式");
        assert_eq!(snap.device_name("zzzzzzzzzzzz"), "zzzzzzzz");
    }

    #[test]
    fn device_presence_carries_weather_and_monitor() {
        let view = DeviceView {
            id: "dev-a".into(),
            name: "书房".into(),
            online: true,
            presence: serde_json::json!({
                "weather": {"city":"绵阳","temp":18.5,"code":1,"text":"多云","at":10},
                "monitor": {"cpu":12.0,"mem":40.0,"disk":55.0,"temp":61.0,"gpu":30.0}
            }),
            ..Default::default()
        };
        assert_eq!(view.weather().unwrap().city, "绵阳");
        assert_eq!(view.weather().unwrap().temp, 18.5);
        assert_eq!(view.monitor().unwrap().cpu, Some(12.0));
        assert_eq!(view.monitor().unwrap().gpu, Some(30.0));
    }
}

#[cfg(test)]
mod music_tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            base_url: "https://nos.example.com".into(),
            token: "t".into(),
            device_id: "dev-b".into(),
            device_name: "宿舍笔记本".into(),
        }
    }

    #[test]
    fn music_body_carries_progress_and_takeover_url() {
        let music = PresenceMusic {
            source: "Daozai.WebDav".into(),
            title: "晴天".into(),
            artist: "周杰伦".into(),
            playing: true,
            position: 42.5,
            duration: 269.0,
            url: "https://nas.local/a.mp3".into(),
        };
        let body = music_body(&cfg(), &music, 7);
        assert_eq!(body["device"], "dev-b");
        assert_eq!(body["name"], "宿舍笔记本");
        assert_eq!(body["title"], "晴天");
        assert_eq!(body["position"], 42.5);
        assert_eq!(body["duration"], 269.0);
        assert_eq!(body["epoch"], 7);
        assert_eq!(body["url"], "https://nas.local/a.mp3");
        assert_eq!(body["playing"], true);
    }

    #[test]
    fn remote_snapshot_parses_every_field_used_by_the_island() {
        let value = serde_json::json!({
            "epoch": 4,
            "now": {
                "device": "dev-a",
                "device_name": "书房台式",
                "title": "十年",
                "artist": "陈奕迅",
                "source": "Daozai.WebDav",
                "url": "https://nas.local/10.mp3",
                "position": 12.0,
                "duration": 200.0,
                "updated_at": 1700000000
            }
        });
        let remote = parse_remote(&value).expect("应解析出远端在播");
        assert_eq!(remote.device, "dev-a");
        assert_eq!(remote.device_name, "书房台式");
        assert_eq!(remote.title, "十年");
        assert_eq!(remote.artist, "陈奕迅");
        assert_eq!(remote.url, "https://nas.local/10.mp3");
        assert_eq!(remote.position, 12.0);
        assert_eq!(remote.duration, 200.0);
    }

    #[test]
    fn nobody_playing_parses_to_none() {
        assert!(parse_remote(&serde_json::json!({"epoch": 2, "now": null})).is_none());
        assert!(parse_remote(&serde_json::json!({"epoch": 0})).is_none());
    }

    #[test]
    fn presence_music_defaults_keep_old_payloads_readable() {
        // 老版本心跳没有 position/url：serde 默认值要让它们仍然能解析。
        let music: PresenceMusic = serde_json::from_str(
            r#"{"source":"QQMusic.exe","title":"歌","artist":"人","playing":true}"#,
        )
        .unwrap();
        assert_eq!(music.position, 0.0);
        assert_eq!(music.duration, 0.0);
        assert!(music.url.is_empty());
    }
}
