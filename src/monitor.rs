//! 系统监控的纯逻辑层：速率差分、单位格式化、今日流量、历史归档、落盘节流与阈值提醒。
//!
//! 本模块刻意与平台无关：不引用任何 Win32 API、不启动线程、不主动写盘、不读系统时钟。
//! 时间一律由调用方以 `Instant` / `Duration` 传入（与 `poll_clock::PollClock::ready_at`
//! 同一套可测写法），日期由调用方以 ISO `YYYY-MM-DD` 字符串传入——本地日期换算属于
//! P2 采集层的职责。Windows 采集层将单独落在 `monitor_win.rs`（`#[cfg(windows)]`）。
//!
//! 设计上修掉参考实现的两处历史缺陷：
//! - 速率用 `Instant` 的真实间隔而非墙钟毫秒差，休眠/校时后不会出现速率尖峰；
//! - 流量以 `u64` 字节累加，不再按 KB 整除存储（小流量会被抹成 0）。

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "monitor.json";
const HISTORY_FILE: &str = "monitor-history.json";
const KIB: u128 = 1024;

/// 历史流量保留天数上限，防止无界增长。
pub const MAX_HISTORY_DAYS: usize = 400;

fn config_path() -> PathBuf {
    crate::model::config_dir().join(CONFIG_FILE)
}

fn history_path() -> PathBuf {
    crate::model::config_dir().join(HISTORY_FILE)
}

/// ISO `YYYY-MM-DD`。只做格式与粗略范围校验，真正的当天日期由采集层生成。
fn iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = [b[0], b[1], b[2], b[3], b[5], b[6], b[8], b[9]];
    if !digits.iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let month = (b[5] - b'0') * 10 + (b[6] - b'0');
    let day = (b[8] - b'0') * 10 + (b[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

// ---------------------------------------------------------------- 速率差分

/// 网卡累计字节计数。字段名直接用 down/up，避免参考实现 in/out 的方向歧义。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub down_bytes: u64,
    pub up_bytes: u64,
}

/// 每秒字节数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Speeds {
    pub down_bps: u64,
    pub up_bps: u64,
}

/// 一次采样的两个产物：`delta` 用于累加今日流量，`speeds` 用于展示。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub delta: Counters,
    pub speeds: Speeds,
}

#[derive(Clone, Debug, Default)]
pub struct RateCalculator {
    last: Option<Counters>,
}

impl RateCalculator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_baseline(&self) -> bool {
        self.last.is_some()
    }

    /// 网卡切换、休眠恢复后调用，避免下一帧把累计差值当成瞬时速率。
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// 首帧、全零计数器、计数器倒退（含 32 位回绕）、零间隔一律返回 0，
    /// 但仍推进基准，使下一个正常采样立刻恢复。
    pub fn step(&mut self, cur: Counters, elapsed: Duration) -> Sample {
        let prev = self.last;
        self.last = Some(cur);
        let Some(prev) = prev else {
            return Sample::default();
        };
        let invalid = (cur.down_bytes == 0 && cur.up_bytes == 0)
            || (prev.down_bytes == 0 && prev.up_bytes == 0)
            || cur.down_bytes < prev.down_bytes
            || cur.up_bytes < prev.up_bytes
            || elapsed.is_zero();
        if invalid {
            return Sample::default();
        }
        let delta = Counters {
            down_bytes: cur.down_bytes - prev.down_bytes,
            up_bytes: cur.up_bytes - prev.up_bytes,
        };
        // 用纳秒做整数换算，任意采样间隔都不依赖毫秒取整，也不会被 u64 溢出击穿。
        let ns = elapsed.as_nanos().max(1);
        let per_second = |bytes: u64| -> u64 {
            ((bytes as u128) * 1_000_000_000u128 / ns).min(u64::MAX as u128) as u64
        };
        Sample {
            delta,
            speeds: Speeds {
                down_bps: per_second(delta.down_bytes),
                up_bps: per_second(delta.up_bytes),
            },
        }
    }
}

// ---------------------------------------------------------------- 单位格式化

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpeedUnit {
    #[default]
    Auto,
    Kilo,
    Mega,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// true = 字节(B)，false = 比特(b)
    pub unit_byte: bool,
    pub speed_unit: SpeedUnit,
    /// 简洁模式：减少小数位、单位省去 B
    pub short_mode: bool,
    pub hide_unit: bool,
    /// 数值与单位之间是否加空格
    pub space: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            unit_byte: true,
            speed_unit: SpeedUnit::Auto,
            short_mode: false,
            hide_unit: false,
            space: true,
        }
    }
}

fn decimals(value: f64, short: bool) -> usize {
    if short {
        if value < 10.0 { 1 } else { 0 }
    } else if value < 10.0 {
        2
    } else if value < 100.0 {
        1
    } else {
        0
    }
}

fn unit_label(level: usize, cfg: &DisplayConfig) -> String {
    let base = match (level, cfg.short_mode) {
        (0, false) => "B",
        (0, true) => "",
        (1, false) => "KB",
        (1, true) => "K",
        (2, false) => "MB",
        (2, true) => "M",
        (3, false) => "GB",
        (3, true) => "G",
        (_, false) => "TB",
        (_, true) => "T",
    };
    if cfg.unit_byte {
        return base.to_string();
    }
    // 比特模式：普通单位把 B 换成 b；简洁模式原本省掉了 B，补一个 b。
    if cfg.short_mode {
        format!("{base}b")
    } else {
        base.replace('B', "b")
    }
}

fn render(scaled: u128, level: usize, cfg: &DisplayConfig) -> String {
    let value = scaled as f64 / 1024f64.powi(level as i32);
    let number = format!("{:.*}", decimals(value, cfg.short_mode), value);
    if cfg.hide_unit {
        return number;
    }
    let sep = if cfg.space { " " } else { "" };
    format!("{number}{sep}{}", unit_label(level, cfg))
}

/// 档位统一按 1024 边界切换（参考实现混用 1024 进制与 1000 阈值，切档处会跳变）。
/// 单位后缀沿用参考实现的省略 `/s` 写法。
pub fn format_speed(bps: u64, cfg: &DisplayConfig) -> String {
    let scaled = (bps as u128).saturating_mul(if cfg.unit_byte { 1 } else { 8 });
    let level = match cfg.speed_unit {
        SpeedUnit::Kilo => 1,
        SpeedUnit::Mega => 2,
        SpeedUnit::Auto => {
            if scaled < KIB {
                0
            } else if scaled < KIB * KIB {
                1
            } else if scaled < KIB * KIB * KIB {
                2
            } else {
                3
            }
        }
    };
    render(scaled, level, cfg)
}

/// 流量/容量格式化，自动升到 TB，不受固定速率档位影响。
pub fn format_size(bytes: u64, cfg: &DisplayConfig) -> String {
    let scaled = (bytes as u128).saturating_mul(if cfg.unit_byte { 1 } else { 8 });
    let level = if scaled < KIB {
        0
    } else if scaled < KIB * KIB {
        1
    } else if scaled < KIB * KIB * KIB {
        2
    } else if scaled < KIB * KIB * KIB * KIB {
        3
    } else {
        4
    };
    render(scaled, level, cfg)
}

// ---------------------------------------------------------------- 今日与历史流量

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DailyTraffic {
    pub date: String,
    pub down_bytes: u64,
    pub up_bytes: u64,
}

impl DailyTraffic {
    pub fn total(&self) -> u64 {
        self.down_bytes.saturating_add(self.up_bytes)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct HistoryFile {
    days: Vec<DailyTraffic>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryStore {
    days: Vec<DailyTraffic>,
}

impl HistoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn days(&self) -> &[DailyTraffic] {
        &self.days
    }

    pub fn today(&self, date: &str) -> Option<&DailyTraffic> {
        self.days.iter().rev().find(|d| d.date == date)
    }

    /// 跨日归档。同一天重复调用无副作用；时钟回拨（传入更早的日期）不新建记录，
    /// 避免把已有数据冲掉。返回是否真的插入了新的一天。
    pub fn roll_to(&mut self, date: &str) -> bool {
        if !iso_date(date) {
            return false;
        }
        if self.days.last().is_some_and(|d| d.date.as_str() >= date) {
            return false;
        }
        self.days.push(DailyTraffic {
            date: date.to_string(),
            ..Default::default()
        });
        self.truncate();
        true
    }

    /// 把一次采样的字节增量累加到指定日期，必要时自动跨日归档。
    pub fn add_delta(&mut self, date: &str, delta: Counters) {
        if !iso_date(date) {
            return;
        }
        self.roll_to(date);
        if let Some(day) = self.days.iter_mut().rev().find(|d| d.date == date) {
            day.down_bytes = day.down_bytes.saturating_add(delta.down_bytes);
            day.up_bytes = day.up_bytes.saturating_add(delta.up_bytes);
        }
    }

    fn truncate(&mut self) {
        if self.days.len() > MAX_HISTORY_DAYS {
            let extra = self.days.len() - MAX_HISTORY_DAYS;
            self.days.drain(..extra);
        }
    }

    /// 最近 `days` 天（含今天）的下行/上行合计，用于日/周/月汇总。
    pub fn totals_since(&self, days: usize) -> Counters {
        self.days.iter().rev().take(days.max(1)).fold(Counters::default(), |mut acc, d| {
            acc.down_bytes = acc.down_bytes.saturating_add(d.down_bytes);
            acc.up_bytes = acc.up_bytes.saturating_add(d.up_bytes);
            acc
        })
    }

    /// 损坏、缺失、非法日期一律降级；重复日期按字节数合并；按日期升序并裁剪上限。
    /// 主文件读不到或解析失败时回退到 `.bak`（进程被强杀后可能留下半截主文件）。
    pub fn load_from(path: &Path) -> Self {
        let primary = std::fs::read(path).ok().and_then(|raw| Self::parse_history(&raw));
        primary
            .or_else(|| std::fs::read(backup_path(path)).ok().and_then(|raw| Self::parse_history(&raw)))
            .unwrap_or_default()
    }

    fn parse_history(raw: &[u8]) -> Option<Self> {
        let file = serde_json::from_slice::<HistoryFile>(raw).ok()?;
        let mut merged: BTreeMap<String, DailyTraffic> = BTreeMap::new();
        for day in file.days.into_iter().filter(|d| iso_date(&d.date)) {
            let entry = merged.entry(day.date.clone()).or_insert_with(|| DailyTraffic {
                date: day.date.clone(),
                ..Default::default()
            });
            entry.down_bytes = entry.down_bytes.saturating_add(day.down_bytes);
            entry.up_bytes = entry.up_bytes.saturating_add(day.up_bytes);
        }
        let mut store = Self {
            days: merged.into_values().collect(),
        };
        store.truncate();
        Some(store)
    }

    /// 显式写盘，走 `model::atomic_write`（临时文件 + sync + 原子替换）。
    /// 托盘退出走的是 `ExitProcess`，不能依赖 Drop，必须由采集层按策略调用。
    /// 写之前把「上一次仍可解析的快照」另存为 `.bak`，供主文件损坏时回退。
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "无法创建监控历史目录".to_string())?;
        }
        let raw = serde_json::to_vec(&HistoryFile {
            days: self.days.clone(),
        })
        .map_err(|_| "无法生成监控历史".to_string())?;
        if let Ok(old) = std::fs::read(path) {
            if Self::parse_history(&old).is_some() {
                let _ = std::fs::copy(path, backup_path(path));
            }
        }
        crate::model::atomic_write(path, &raw).map_err(|_| "无法保存监控历史".to_string())
    }

    pub fn load() -> Self {
        Self::load_from(&history_path())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&history_path())
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// 落盘节流：首次只建立基线；之后需同时满足「距上次保存 ≥ 间隔」与「今日流量增量 ≥ 阈值」。
#[derive(Clone, Debug)]
pub struct SavePolicy {
    pub min_interval: Duration,
    pub min_delta_bytes: u64,
    last: Option<(Instant, u64)>,
}

impl Default for SavePolicy {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(30),
            min_delta_bytes: 10 * 1024 * 1024,
            last: None,
        }
    }
}

impl SavePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// 跨日清零时调用（或直接依赖 `today_bytes < last_bytes` 的自动重置）。
    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn should_save(&mut self, now: Instant, today_bytes: u64) -> bool {
        let Some((at, bytes)) = self.last else {
            self.last = Some((now, today_bytes));
            return false;
        };
        if today_bytes < bytes {
            self.last = Some((now, today_bytes));
            return false;
        }
        if now.saturating_duration_since(at) < self.min_interval {
            return false;
        }
        if today_bytes - bytes < self.min_delta_bytes {
            return false;
        }
        self.last = Some((now, today_bytes));
        true
    }
}

// ---------------------------------------------------------------- 阈值提醒

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// 0 表示关闭
    pub today_traffic_bytes: u64,
    /// 0 表示关闭
    pub cpu_percent: u8,
    /// 0 表示关闭
    pub mem_percent: u8,
    pub cooldown_secs: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            today_traffic_bytes: 0,
            cpu_percent: 0,
            mem_percent: 0,
            cooldown_secs: 300,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    TodayTraffic,
    Cpu,
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    pub value: f64,
}

impl Alert {
    pub fn message(&self, cfg: &DisplayConfig) -> String {
        match self.kind {
            AlertKind::TodayTraffic => {
                format!("今日流量已超过 {}", format_size(self.value as u64, cfg))
            }
            AlertKind::Cpu => format!("CPU 占用 {:.0}%", self.value),
            AlertKind::Memory => format!("内存占用 {:.0}%", self.value),
        }
    }
}

/// 越限只提醒一次，恢复后重新武装；冷却期内不刷屏。
#[derive(Clone, Debug)]
pub struct ThresholdWatcher {
    cooldown: Duration,
    last: Option<Instant>,
    active: Option<AlertKind>,
}

impl ThresholdWatcher {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last: None,
            active: None,
        }
    }

    pub fn check(
        &mut self,
        now: Instant,
        today_bytes: u64,
        cpu: Option<f32>,
        mem: Option<f32>,
        cfg: &Thresholds,
    ) -> Option<Alert> {
        let current = if cfg.today_traffic_bytes > 0 && today_bytes >= cfg.today_traffic_bytes {
            Some(Alert {
                kind: AlertKind::TodayTraffic,
                value: today_bytes as f64,
            })
        } else if cfg.cpu_percent > 0 && cpu.is_some_and(|v| v >= cfg.cpu_percent as f32) {
            Some(Alert {
                kind: AlertKind::Cpu,
                value: cpu.unwrap_or(0.0) as f64,
            })
        } else if cfg.mem_percent > 0 && mem.is_some_and(|v| v >= cfg.mem_percent as f32) {
            Some(Alert {
                kind: AlertKind::Memory,
                value: mem.unwrap_or(0.0) as f64,
            })
        } else {
            None
        };
        let Some(alert) = current else {
            self.active = None;
            return None;
        };
        if self.active == Some(alert.kind) {
            return None;
        }
        if self
            .last
            .is_some_and(|at| now.saturating_duration_since(at) < self.cooldown)
        {
            return None;
        }
        self.active = Some(alert.kind);
        self.last = Some(now);
        Some(alert)
    }

    pub fn reset(&mut self) {
        self.active = None;
    }
}

// ---------------------------------------------------------------- 配置与快照 DTO

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub enabled: bool,
    pub interval_ms: u64,
    /// 0 = 统计全部活动网卡；非 0 = 只统计该 InterfaceIndex（P4 多网卡选择）。
    pub interface_index: u32,
    pub display: DisplayConfig,
    pub thresholds: Thresholds,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 1000,
            interface_index: 0,
            display: DisplayConfig::default(),
            thresholds: Thresholds::default(),
        }
    }
}

impl MonitorConfig {
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "无法创建监控配置目录".to_string())?;
        }
        let raw = serde_json::to_vec_pretty(self).map_err(|_| "无法生成监控配置".to_string())?;
        crate::model::atomic_write(path, &raw).map_err(|_| "无法保存监控配置".to_string())
    }

    /// 采样间隔夹到 250ms–60s，避免配置写坏导致忙轮询。
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.clamp(250, 60_000))
    }
}

/// P2 采集层与 P3 `app::Env` 之间的公共 DTO。先定好字段，避免后面改两遍。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MonitorSnapshot {
    pub down_bps: u64,
    pub up_bps: u64,
    pub cpu_percent: Option<f32>,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub today_down_bytes: u64,
    pub today_up_bytes: u64,
    /// P5：PDH / 电源 API 读取的扩展硬件指标，不可用时为 None。
    pub gpu_percent: Option<f32>,
    pub disk_percent: Option<f32>,
    pub cpu_mhz: Option<u32>,
    /// 全盘读写吞吐（MB/s），仪表盘「硬盘」卡片用。
    pub disk_read_mbps: Option<f32>,
    pub disk_write_mbps: Option<f32>,
}

impl MonitorSnapshot {
    pub fn mem_percent(&self) -> Option<f32> {
        (self.mem_total_bytes > 0)
            .then(|| self.mem_used_bytes as f32 * 100.0 / self.mem_total_bytes as f32)
    }

    pub fn today_total(&self) -> u64 {
        self.today_down_bytes.saturating_add(self.today_up_bytes)
    }
}

/// 占用率迷你图的固定长度环形缓冲（P5）。纯逻辑、平台无关，值统一夹到 0..100。
pub const SPARK_POINTS: usize = 48;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sparkline {
    points: VecDeque<f32>,
}

impl Sparkline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        if self.points.len() == SPARK_POINTS {
            self.points.pop_front();
        }
        self.points.push_back(value.clamp(0.0, 100.0));
    }

    pub fn points(&self) -> impl Iterator<Item = f32> + '_ {
        self.points.iter().copied()
    }

    pub fn last(&self) -> Option<f32> {
        self.points.back().copied()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }
}

/// 一次完整的趋势采样。仪表盘底部的六列历史和折线都取自它，
/// 而不是各指标各存一条曲线——这样时间轴天然对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrendSample {
    /// 采样时刻（本地时区 Unix 秒），用于列标签。
    pub at: i64,
    pub cpu: f32,
    pub mem: f32,
    pub gpu: f32,
    pub disk: f32,
    /// 字节/秒。网络量级与占用率差几个数量级，所以不夹取上限。
    pub down: f32,
    pub up: f32,
}

impl TrendSample {
    /// 四项都拿不到时没有意义，不入库（避免曲线被一串 0 拉平）。
    fn usable(&self) -> bool {
        [self.cpu, self.mem, self.gpu, self.disk, self.down, self.up]
            .iter()
            .any(|v| v.is_finite() && *v > 0.0)
    }
}

/// 固定容量趋势缓冲。采样间隔 1 秒、容量 720，约覆盖 12 分钟。
#[derive(Clone, Debug)]
pub struct Trend {
    samples: VecDeque<TrendSample>,
}

impl Default for Trend {
    fn default() -> Self {
        Self { samples: VecDeque::with_capacity(Trend::CAP) }
    }
}

impl Trend {
    pub const CAP: usize = 720;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, sample: TrendSample) {
        if !sample.usable() {
            return;
        }
        if self.samples.len() == Self::CAP {
            self.samples.pop_front();
        }
        self.samples.push_back(TrendSample {
            cpu: sample.cpu.max(0.0),
            mem: sample.mem.clamp(0.0, 100.0),
            gpu: sample.gpu.clamp(0.0, 100.0),
            disk: sample.disk.clamp(0.0, 100.0),
            down: sample.down.max(0.0),
            up: sample.up.max(0.0),
            at: sample.at,
        });
    }

    /// 均匀取 `n` 个点，**最后一个一定是最新样本**；样本不足时有多少给多少。
    /// `n == 1` 时返回最新那个。
    pub fn pick(&self, n: usize) -> Vec<TrendSample> {
        let len = self.samples.len();
        if n == 0 || len == 0 {
            return Vec::new();
        }
        if n == 1 {
            return vec![self.samples[len - 1]];
        }
        if len <= n {
            return self.samples.iter().copied().collect();
        }
        (0..n)
            .map(|i| self.samples[(len - 1) * i / (n - 1)])
            .collect()
    }

    pub fn back(&self) -> Option<TrendSample> {
        self.samples.back().copied()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// 计算机名，用于仪表盘上的「本机 · xxx」。取不到时退回「本机」。
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "本机".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "daozai-monitor-{}-{}-{}.json",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn serial_date(n: usize) -> String {
        format!(
            "{:04}-{:02}-{:02}",
            2020 + n / 360,
            1 + (n % 360) / 30,
            1 + n % 30
        )
    }

    #[test]
    fn first_sample_has_no_spike_but_sets_baseline() {
        let mut r = RateCalculator::new();
        assert_eq!(
            r.step(
                Counters {
                    down_bytes: 1_000,
                    up_bytes: 200
                },
                Duration::from_secs(1)
            ),
            Sample::default()
        );
        assert!(r.has_baseline());
        let s = r.step(
            Counters {
                down_bytes: 2_000,
                up_bytes: 700
            },
            Duration::from_secs(1),
        );
        assert_eq!(
            s.delta,
            Counters {
                down_bytes: 1000,
                up_bytes: 500
            }
        );
        assert_eq!(s.speeds, Speeds { down_bps: 1000, up_bps: 500 });
    }

    #[test]
    fn speed_uses_real_elapsed_not_configured_interval() {
        let mut r = RateCalculator::new();
        r.step(
            Counters {
                down_bytes: 1,
                up_bytes: 1
            },
            Duration::from_secs(1),
        );
        let s = r.step(
            Counters {
                down_bytes: 1 + 1500,
                up_bytes: 1 + 3000
            },
            Duration::from_millis(1500),
        );
        assert_eq!(s.speeds.down_bps, 1000);
        assert_eq!(s.speeds.up_bps, 2000);
        let s = r.step(
            Counters {
                down_bytes: 1 + 1500 + 500,
                up_bytes: 1 + 3000
            },
            Duration::from_millis(250),
        );
        assert_eq!(s.speeds.down_bps, 2000);
        assert_eq!(s.speeds.up_bps, 0);
    }

    #[test]
    fn counter_rollback_is_rejected_and_baseline_advances() {
        let mut r = RateCalculator::new();
        r.step(
            Counters {
                down_bytes: u32::MAX as u64,
                up_bytes: 10
            },
            Duration::from_secs(1),
        );
        // 模拟 32 位计数器回绕 / 网卡重置
        assert_eq!(
            r.step(
                Counters {
                    down_bytes: 256,
                    up_bytes: 20
                },
                Duration::from_secs(1)
            ),
            Sample::default()
        );
        let s = r.step(
            Counters {
                down_bytes: 256 + 2048,
                up_bytes: 40
            },
            Duration::from_secs(1),
        );
        assert_eq!(s.speeds.down_bps, 2048);
        assert_eq!(s.speeds.up_bps, 20);
    }

    #[test]
    fn zero_counters_and_zero_interval_do_not_produce_speed() {
        let mut r = RateCalculator::new();
        r.step(
            Counters {
                down_bytes: 500,
                up_bytes: 500
            },
            Duration::from_secs(1),
        );
        assert_eq!(r.step(Counters::default(), Duration::from_secs(1)), Sample::default());
        r.reset();
        r.step(
            Counters {
                down_bytes: 10,
                up_bytes: 10
            },
            Duration::from_secs(1),
        );
        assert_eq!(
            r.step(
                Counters {
                    down_bytes: 20,
                    up_bytes: 30
                },
                Duration::ZERO
            ),
            Sample::default()
        );
    }

    #[test]
    fn reset_after_adapter_switch_prevents_spike() {
        let mut r = RateCalculator::new();
        r.step(
            Counters {
                down_bytes: 10_000_000,
                up_bytes: 10_000_000
            },
            Duration::from_secs(1),
        );
        r.reset();
        assert!(!r.has_baseline());
        assert_eq!(
            r.step(
                Counters {
                    down_bytes: 42,
                    up_bytes: 7
                },
                Duration::from_secs(1)
            ),
            Sample::default()
        );
    }

    #[test]
    fn rate_delta_feeds_history_without_losing_bytes() {
        let mut rate = RateCalculator::new();
        let mut history = HistoryStore::new();
        let mut counters = Counters {
            down_bytes: 1,
            up_bytes: 1,
        };
        rate.step(counters, Duration::from_secs(1));
        for _ in 0..3 {
            counters.down_bytes += 512;
            counters.up_bytes += 256;
            let sample = rate.step(counters, Duration::from_secs(1));
            history.add_delta("2026-11-14", sample.delta);
        }
        let today = history.today("2026-11-14").unwrap();
        assert_eq!(today.down_bytes, 1536);
        assert_eq!(today.up_bytes, 768);
        assert_eq!(today.total(), 2304);
    }

    #[test]
    fn format_speed_auto_boundaries_are_1024_based() {
        let c = DisplayConfig::default();
        assert_eq!(format_speed(1023, &c), "1023 B");
        assert_eq!(format_speed(1024, &c), "1.00 KB");
        assert_eq!(format_speed(1024 * 1024, &c), "1.00 MB");
        assert_eq!(format_speed(1024 * 1024 * 1024, &c), "1.00 GB");
    }

    #[test]
    fn bit_short_hide_and_space_variants() {
        let mut c = DisplayConfig::default();
        c.unit_byte = false;
        assert_eq!(format_speed(1024, &c), "8.00 Kb");
        c.short_mode = true;
        assert_eq!(format_speed(1024, &c), "8.0 Kb");
        c.hide_unit = true;
        assert_eq!(format_speed(1024, &c), "8.0");
        c.hide_unit = false;
        c.space = false;
        assert_eq!(format_speed(1024, &c), "8.0Kb");
        c.unit_byte = true;
        c.short_mode = false;
        c.space = true;
        assert_eq!(format_speed(1024, &c), "1.00 KB");
    }

    #[test]
    fn fixed_units_and_huge_values_do_not_panic() {
        let mut c = DisplayConfig::default();
        c.speed_unit = SpeedUnit::Kilo;
        assert_eq!(format_speed(2 * 1024 * 1024, &c), "2048 KB");
        c.speed_unit = SpeedUnit::Mega;
        assert_eq!(format_speed(2 * 1024 * 1024, &c), "2.00 MB");
        assert!(!format_speed(u64::MAX, &c).is_empty());
    }

    #[test]
    fn format_size_scales_to_tb() {
        let c = DisplayConfig::default();
        assert_eq!(format_size(1023, &c), "1023 B");
        assert_eq!(format_size(1u64 << 40, &c), "1.00 TB");
        assert!(!format_size(u64::MAX, &c).is_empty());
    }

    #[test]
    fn history_rolls_across_month_year_and_ignores_clock_rollback() {
        let mut h = HistoryStore::new();
        assert!(h.roll_to("2026-11-13"));
        h.add_delta(
            "2026-11-13",
            Counters {
                down_bytes: 100,
                up_bytes: 20,
            },
        );
        assert!(h.roll_to("2026-12-01"));
        assert!(h.roll_to("2027-01-01"));
        assert_eq!(h.days().len(), 3);
        assert_eq!(h.today("2026-11-13").unwrap().down_bytes, 100);
        assert!(!h.roll_to("2026-06-01"));
        assert!(!h.roll_to("2027-01-01"));
        assert!(!h.roll_to("not-a-date"));
        assert!(!h.roll_to("2026-13-40"));
        assert_eq!(h.days().len(), 3);
    }

    #[test]
    fn history_keeps_byte_precision() {
        let mut h = HistoryStore::new();
        for _ in 0..1000 {
            h.add_delta(
                "2026-11-14",
                Counters {
                    down_bytes: 1,
                    up_bytes: 0,
                },
            );
        }
        assert_eq!(h.today("2026-11-14").unwrap().down_bytes, 1000);
    }

    #[test]
    fn load_merges_duplicate_days_drops_bad_dates_and_caps() {
        let path = tmp("merge");
        let file = HistoryFile {
            days: vec![
                DailyTraffic {
                    date: "2026-11-14".into(),
                    down_bytes: 100,
                    up_bytes: 10,
                },
                DailyTraffic {
                    date: "2026-11-14".into(),
                    down_bytes: 50,
                    up_bytes: 5,
                },
                DailyTraffic {
                    date: "bad".into(),
                    down_bytes: 999,
                    up_bytes: 999,
                },
                DailyTraffic {
                    date: "2026-11-13".into(),
                    down_bytes: 7,
                    up_bytes: 0,
                },
            ],
        };
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let h = HistoryStore::load_from(&path);
        assert_eq!(h.days().len(), 2);
        assert_eq!(h.days()[0].date, "2026-11-13");
        assert_eq!(h.days()[1].down_bytes, 150);
        assert_eq!(h.days()[1].up_bytes, 15);
        let _ = std::fs::remove_file(&path);

        let mut big = HistoryStore::new();
        for i in 0..(MAX_HISTORY_DAYS + 50) {
            big.add_delta(&serial_date(i), Counters::default());
        }
        assert_eq!(big.days().len(), MAX_HISTORY_DAYS);
        assert_eq!(big.days().last().unwrap().date, serial_date(MAX_HISTORY_DAYS + 49));
    }

    #[test]
    fn corrupt_or_missing_history_file_degrades_to_empty() {
        let p = tmp("corrupt");
        std::fs::write(&p, b"not json {{{").unwrap();
        assert!(HistoryStore::load_from(&p).days().is_empty());
        let _ = std::fs::remove_file(&p);
        assert!(HistoryStore::load_from(&tmp("missing")).days().is_empty());
    }

    #[test]
    fn corrupt_primary_falls_back_to_last_valid_backup() {
        let p = tmp("bak-restore");
        let mut h = HistoryStore::new();
        h.add_delta("2026-11-14", Counters { down_bytes: 4096, up_bytes: 1024 });
        h.save_to(&p).unwrap();
        h.add_delta("2026-11-14", Counters { down_bytes: 2048, up_bytes: 512 });
        h.save_to(&p).unwrap(); // 上一版（4096）被另存为 .bak
        std::fs::write(&p, b"half-written {{{").unwrap();
        let restored = HistoryStore::load_from(&p);
        assert_eq!(restored.today("2026-11-14").unwrap().down_bytes, 4096);
        let _ = std::fs::remove_file(backup_path(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn saving_over_a_corrupt_primary_keeps_the_backup_valid() {
        let p = tmp("bak-guard");
        let mut h = HistoryStore::new();
        h.add_delta("2026-11-14", Counters { down_bytes: 512, up_bytes: 0 });
        h.save_to(&p).unwrap();
        h.add_delta("2026-11-14", Counters { down_bytes: 256, up_bytes: 0 });
        h.save_to(&p).unwrap(); // 上一版（512）成为 .bak
        std::fs::write(&p, b"broken").unwrap();
        h.add_delta("2026-11-14", Counters { down_bytes: 128, up_bytes: 0 });
        h.save_to(&p).unwrap(); // 坏主文件不应覆盖已有的有效 .bak
        std::fs::write(&p, b"broken again").unwrap();
        assert_eq!(HistoryStore::load_from(&p).today("2026-11-14").unwrap().down_bytes, 512);
        let _ = std::fs::remove_file(backup_path(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn totals_since_aggregates_recent_days() {
        let mut h = HistoryStore::new();
        for (date, down, up) in [("2026-11-12", 100, 10), ("2026-11-13", 200, 20), ("2026-11-14", 300, 30)] {
            h.add_delta(date, Counters { down_bytes: down, up_bytes: up });
        }
        assert_eq!(h.totals_since(1), Counters { down_bytes: 300, up_bytes: 30 });
        assert_eq!(h.totals_since(2), Counters { down_bytes: 500, up_bytes: 50 });
        assert_eq!(h.totals_since(30), Counters { down_bytes: 600, up_bytes: 60 });
    }

    #[test]
    fn sparkline_caps_history_and_rejects_non_finite() {
        let mut s = Sparkline::new();
        for i in 0..(SPARK_POINTS + 20) {
            s.push(i as f32);
        }
        assert_eq!(s.len(), SPARK_POINTS);
        assert_eq!(s.last(), Some((SPARK_POINTS + 19) as f32));
        s.push(f32::NAN);
        s.push(f32::INFINITY);
        assert_eq!(s.len(), SPARK_POINTS);
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn sparkline_clamps_out_of_range_values() {
        let mut s = Sparkline::new();
        s.push(-5.0);
        s.push(150.0);
        assert_eq!(s.points().collect::<Vec<_>>(), vec![0.0, 100.0]);
    }

    #[test]
    fn config_default_uses_all_adapters() {
        assert_eq!(MonitorConfig::default().interface_index, 0);
        let p = tmp("iface");
        std::fs::write(&p, br#"{"interface_index": 7}"#).unwrap();
        assert_eq!(MonitorConfig::load_from(&p).interface_index, 7);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn history_roundtrip_through_atomic_write() {
        let p = tmp("roundtrip");
        let mut h = HistoryStore::new();
        h.add_delta(
            "2026-11-14",
            Counters {
                down_bytes: 12_345,
                up_bytes: 678,
            },
        );
        h.save_to(&p).unwrap();
        assert_eq!(HistoryStore::load_from(&p), h);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_policy_needs_interval_and_delta_and_resets_on_rollover() {
        let t0 = Instant::now();
        let mib = 1024 * 1024;
        let mut p = SavePolicy::new();
        assert!(!p.should_save(t0, 0));
        assert!(!p.should_save(t0 + Duration::from_secs(10), 10 * mib));
        assert!(!p.should_save(t0 + Duration::from_secs(31), 0));
        assert!(!p.should_save(t0 + Duration::from_secs(32), 1024));
        assert!(!p.should_save(t0 + Duration::from_secs(62), 1024));
        assert!(p.should_save(t0 + Duration::from_secs(62), 1024 + 10 * mib));
        assert!(!p.should_save(t0 + Duration::from_secs(63), 1024 + 10 * mib));
        assert!(!p.should_save(t0 + Duration::from_secs(64), 0));
        p.reset();
        assert!(!p.should_save(t0 + Duration::from_secs(65), 0));
    }

    #[test]
    fn thresholds_fire_once_cooldown_and_rearm() {
        let t0 = Instant::now();
        let mut w = ThresholdWatcher::new(Duration::from_secs(300));
        let cfg = Thresholds {
            cpu_percent: 90,
            ..Thresholds::default()
        };
        assert_eq!(
            w.check(t0, 0, Some(95.0), None, &cfg).map(|a| a.kind),
            Some(AlertKind::Cpu)
        );
        assert!(w.check(t0 + Duration::from_secs(10), 0, Some(99.0), None, &cfg).is_none());
        assert!(w.check(t0 + Duration::from_secs(20), 0, Some(50.0), None, &cfg).is_none());
        assert!(w.check(t0 + Duration::from_secs(21), 0, Some(95.0), None, &cfg).is_none());
        assert_eq!(
            w.check(t0 + Duration::from_secs(301), 0, Some(95.0), None, &cfg)
                .map(|a| a.kind),
            Some(AlertKind::Cpu)
        );
    }

    #[test]
    fn alert_message_is_chinese_and_readable() {
        let a = Alert {
            kind: AlertKind::TodayTraffic,
            value: (2u64 << 30) as f64,
        };
        assert_eq!(a.message(&DisplayConfig::default()), "今日流量已超过 2.00 GB");
        assert_eq!(
            Alert {
                kind: AlertKind::Memory,
                value: 91.4
            }
            .message(&DisplayConfig::default()),
            "内存占用 91%"
        );
    }

    #[test]
    fn config_defaults_survive_partial_json_and_clamp_interval() {
        let p = tmp("config");
        std::fs::write(&p, br#"{"interval_ms": 5}"#).unwrap();
        let c = MonitorConfig::load_from(&p);
        assert!(c.enabled);
        assert_eq!(c.display.speed_unit, SpeedUnit::Auto);
        assert_eq!(c.interval(), Duration::from_millis(250));
        assert_eq!(c.thresholds.cooldown_secs, 300);
        c.save_to(&p).unwrap();
        assert_eq!(MonitorConfig::load_from(&p), c);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn snapshot_mem_percent_handles_zero_total() {
        let mut s = MonitorSnapshot::default();
        assert_eq!(s.mem_percent(), None);
        s.mem_used_bytes = 4u64 << 30;
        s.mem_total_bytes = 8u64 << 30;
        assert_eq!(s.mem_percent(), Some(50.0));
        s.today_down_bytes = 100;
        s.today_up_bytes = 20;
        assert_eq!(s.today_total(), 120);
    }

    fn trend_sample(i: i64) -> TrendSample {
        TrendSample { at: i, cpu: i as f32, mem: 50.0, gpu: 10.0, disk: 5.0, down: 1024.0, up: 256.0 }
    }

    #[test]
    fn trend_caps_oldest_and_ignores_empty_samples() {
        let mut t = Trend::new();
        assert!(t.is_empty());
        // 全零样本视为无效，不入库。
        t.push(TrendSample::default());
        assert!(t.is_empty());
        for i in 0..(Trend::CAP as i64 + 5) {
            t.push(trend_sample(i));
        }
        assert_eq!(t.len(), Trend::CAP);
        // 最早的 5 条被挤掉，末尾是最新一条。
        assert_eq!(t.back().map(|s| s.at), Some(Trend::CAP as i64 + 4));
        t.clear();
        assert!(t.is_empty());
    }

    #[test]
    fn trend_pick_keeps_newest_and_is_uniform() {
        let mut t = Trend::new();
        assert!(t.pick(6).is_empty());
        for i in 0..60 {
            t.push(trend_sample(i));
        }
        let six = t.pick(6);
        assert_eq!(six.len(), 6);
        assert_eq!(six.last().map(|s| s.at), Some(59), "最后一点必须是最新样本");
        assert_eq!(six.first().map(|s| s.at), Some(0));
        // 6 个点把 0..=59 均匀切开；整数除法允许相邻间距差 1。
        let steps: Vec<i64> = six.windows(2).map(|w| w[1].at - w[0].at).collect();
        let (lo, hi) = (steps.iter().min().copied().unwrap(), steps.iter().max().copied().unwrap());
        assert!(hi - lo <= 1, "列间距应基本均匀：{steps:?}");
        // 样本不足时有多少给多少，不补零。
        let mut few = Trend::new();
        few.push(trend_sample(7));
        assert_eq!(few.pick(6), vec![trend_sample(7)]);
        assert_eq!(few.pick(1).len(), 1);
        assert!(few.pick(0).is_empty());
    }

    #[test]
    fn trend_clamps_percentages_but_not_rates() {
        let mut t = Trend::new();
        t.push(TrendSample {
            at: 1,
            cpu: -5.0,
            mem: 250.0,
            gpu: 130.0,
            disk: -1.0,
            down: 9.9e9,
            up: -3.0,
        });
        let s = t.back().unwrap();
        assert_eq!(s.cpu, 0.0);
        assert_eq!(s.mem, 100.0);
        assert_eq!(s.gpu, 100.0);
        assert_eq!(s.disk, 0.0);
        assert_eq!(s.up, 0.0);
        assert_eq!(s.down, 9.9e9, "网络速率不能夹到 100，否则曲线会被压平");
    }

    #[test]
    fn machine_name_is_never_blank() {
        assert!(!machine_name().trim().is_empty());
    }
}
