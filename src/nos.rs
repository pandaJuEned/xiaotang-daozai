//! 课表：读取「不止课表」（NOS）的课表事件流。
//!
//! 数据来自 NOS Web 端的 `GET /api/daozai/agenda`。**服务端已经把节次制排课
//! 展开成绝对时刻**，本模块只做三件事：
//!
//! 1. 解析 JSON（`parse`）；
//! 2. 把「本地墙钟」换算成本机时间戳（`wall_clock`）；
//! 3. 选课程、找出下一节课（`next_course` / `today`）。
//!
//! **不在本模块重写排课算法**（周次、单双周、周集合、节次→钟点）。那是
//! NOS `calc.py` 的职责，两份实现各自漂移会让「今天 3 节课显示成 2 节」这类
//! 静默错误变得极难排查。
//!
//! 时间语义：NOS 返回的是**本地墙钟**（`date` + `HH:mm`），因为服务器时区
//! 未必与使用者一致（Debian 上多为 UTC）。由本机按本地时区解释。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant};

use eframe::egui;
use crate::poll_clock::PollClock;

/// 自动刷新间隔（秒）：课表一周内基本不变，5 分钟足够。
pub const INTERVAL: i64 = 300;
/// 一次拉取覆盖的天数：今天起一周，够看「今天 + 本周剩余」。
pub const WINDOW_DAYS: i64 = 7;
/// 失败退避的最大间隔（秒）。
const MAX_BACKOFF: f64 = 1800.0;
/// 首次失败后的退避基数（秒）。
const BACKOFF_BASE: f64 = 120.0;

// ---------------------------------------------------------------- 数据模型

/// 一条课表事件（服务端已展开成具体日期与时刻）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// 稳定标识；同一条课在多次拉取间保持不变。
    pub uid: String,
    /// `course`（课程）/ `meeting`（会议）/ `exam`（期末考试）。
    pub kind: String,
    pub title: String,
    /// `YYYY-MM-DD`。
    pub date: String,
    /// `HH:mm`；空串表示全天事件。
    #[serde(default)]
    pub start_time: String,
    /// `HH:mm`；空串表示结束时间未知。
    #[serde(default)]
    pub end_time: String,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub teacher: String,
    #[serde(default)]
    pub course_id: String,
    #[serde(default)]
    pub semester_id: String,
    /// 教学周号；非排课事件为空。
    #[serde(default)]
    pub week: Option<i64>,
}

impl Event {
    /// 是否是课程（不含会议与考试）。
    pub fn is_course(&self) -> bool {
        self.kind == "course"
    }

    /// 是否是考试。
    pub fn is_exam(&self) -> bool {
        self.kind == "exam"
    }

    /// 上课地点；没有地点时回退到教师名，都没有则空串。
    pub fn place(&self) -> String {
        let location = self.location.trim();
        if !location.is_empty() {
            return location.to_string();
        }
        self.teacher.trim().to_string()
    }
}

/// 学期信息。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Semester {
    pub id: String,
    pub name: String,
    /// `monday` / `sunday`。
    #[serde(default)]
    pub week_start_day: String,
}

/// 一次拉取的结果。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Agenda {
    #[serde(default)]
    pub semesters: Vec<Semester>,
    #[serde(default)]
    pub events: Vec<Event>,
    /// 取得这份数据的 Unix 时间（秒）。
    #[serde(default)]
    pub at: i64,
}

impl Agenda {
    /// 当前学期：优先取与服务端一致的「今天所在学期」；这里按学期开始日期倒序取最近一个。
    pub fn current_semester(&self) -> Option<&Semester> {
        self.semesters.last()
    }

    /// 某一天的事件（按开始时刻排序）。`day` 为 `YYYY-MM-DD`。
    pub fn day(&self, day: &str) -> Vec<&Event> {
        let mut events: Vec<&Event> = self.events.iter().filter(|e| e.date == day).collect();
        events.sort_by(|a, b| {
            a.all_day
                .cmp(&b.all_day)
                .then(a.start_time.cmp(&b.start_time))
                .then(a.title.cmp(&b.title))
        });
        events
    }

    /// 今天的课程（不含会议与考试），按开始时刻排序。
    pub fn today_courses(&self, day: &str) -> Vec<&Event> {
        self.day(day).into_iter().filter(|e| e.is_course()).collect()
    }

    /// 下一节课：从 `now`（Unix 秒）起，最近的一条尚未开始的课程。
    ///
    /// 全天事件不参与「下一节课」——它们没有开始时刻。
    pub fn next_course(&self, now: i64) -> Option<&Event> {
        self.events
            .iter()
            .filter(|e| e.is_course() && !e.all_day && !e.start_time.is_empty())
            .filter_map(|e| e.start_at().map(|start| (start, e)))
            .filter(|(start, _)| *start >= now)
            .min_by_key(|(start, e)| (*start, e.title.clone()))
            .map(|(_, e)| e)
    }

    /// 当前正在上的课（`now` 落在开始与结束之间）。
    pub fn current_course(&self, now: i64) -> Option<&Event> {
        self.events
            .iter()
            .filter(|e| e.is_course() && !e.all_day)
            .filter(|e| match (e.start_at(), e.end_at()) {
                (Some(start), Some(end)) => now >= start && now < end,
                (Some(start), None) => now >= start && now < start + 3600,
                _ => false,
            })
            .min_by_key(|e| e.start_time.clone())
    }
}

impl Event {
    /// 开始时刻（Unix 秒，本机本地时区解释）。全天或时间缺失时为空。
    pub fn start_at(&self) -> Option<i64> {
        wall_clock(&self.date, &self.start_time)
    }

    /// 结束时刻（Unix 秒）。时间缺失时为空 —— **不推测时长**。
    pub fn end_at(&self) -> Option<i64> {
        wall_clock(&self.date, &self.end_time)
    }
}

// ---------------------------------------------------------------- 时间换算

/// 把「本地墙钟」（`YYYY-MM-DD` + `HH:mm`）换算成 Unix 秒，按本机本地时区解释。
///
/// 时区歧义（夏令时切换那一小时有两个时刻）时返回 `None` —— 宁可少显示，
/// 不可显示错。
pub fn wall_clock(date: &str, time: &str) -> Option<i64> {
    let time = time.trim();
    if time.is_empty() {
        return None;
    }
    let date = chrono::NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let time = chrono::NaiveTime::parse_from_str(time, "%H:%M").ok()?;
    let naive = date.and_time(time);
    naive
        .and_local_timezone(chrono::Local)
        .single()
        .map(|dt| dt.timestamp())
}

/// `HH:mm` → 显示用 `HH:mm`（规整成两位）。
pub fn clock_label(time: &str) -> String {
    if time.trim().is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = time.trim().split(':').collect();
    if parts.len() < 2 {
        return time.trim().to_string();
    }
    match (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
        (Ok(h), Ok(m)) if h < 24 && m < 60 => format!("{h:02}:{m:02}"),
        _ => time.trim().to_string(),
    }
}

// ---------------------------------------------------------------- 解析

/// 解析服务端返回的 agenda JSON。
///
/// 宽容处理：单条事件字段缺失时跳过该条，不让一条脏数据毁掉整份课表。
pub fn parse(body: &str, at: i64) -> Result<Agenda, String> {
    let value: Value = serde_json::from_str(body).map_err(|_| "课表响应格式无效".to_string())?;
    let mut agenda = Agenda {
        at,
        ..Default::default()
    };
    if let Some(items) = value["semesters"].as_array() {
        for item in items {
            let id = item["id"].as_str().unwrap_or_default().to_string();
            if id.is_empty() {
                continue;
            }
            agenda.semesters.push(Semester {
                id,
                name: item["name"].as_str().unwrap_or_default().to_string(),
                week_start_day: item["week_start_day"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    if let Some(items) = value["events"].as_array() {
        for item in items {
            let uid = item["uid"].as_str().unwrap_or_default().to_string();
            let title = item["title"].as_str().unwrap_or_default().trim().to_string();
            let date = item["date"].as_str().unwrap_or_default().trim().to_string();
            if uid.is_empty() || title.is_empty() || date.is_empty() {
                continue;
            }
            if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
                continue;
            }
            agenda.events.push(Event {
                uid,
                kind: item["kind"].as_str().unwrap_or("course").to_string(),
                title,
                date,
                start_time: item["start_time"].as_str().unwrap_or_default().trim().to_string(),
                end_time: item["end_time"].as_str().unwrap_or_default().trim().to_string(),
                all_day: item["all_day"].as_bool().unwrap_or(false),
                location: item["location"].as_str().unwrap_or_default().trim().to_string(),
                color: item["color"].as_str().unwrap_or_default().trim().to_string(),
                teacher: item["teacher"].as_str().unwrap_or_default().trim().to_string(),
                course_id: item["course_id"].as_str().unwrap_or_default().trim().to_string(),
                semester_id: item["semester_id"].as_str().unwrap_or_default().trim().to_string(),
                week: item["week"].as_i64(),
            });
        }
    }
    Ok(agenda)
}

// ---------------------------------------------------------------- 拉取

/// 构造 agenda 请求地址。
pub fn agenda_url(base: &str, from: &str, to: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(base.trim())
        .map_err(|_| "课表服务器地址无效".to_string())?
        // 用户填的是站点根；必须落到聚合接口。否则请求打到静态首页拿到 HTML，
        // 再按 JSON 解析就只会报含糊的「课表响应格式无效」。
        .join("/api/daozai/agenda")
        .map_err(|_| "课表服务器地址无效".to_string())?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("from", from);
        query.append_pair("to", to);
    }
    Ok(url)
}

/// 拉一次课表。`token` 是 Bearer 令牌（不含 "Bearer " 前缀）。
pub fn fetch(agent: &ureq::Agent, base: &str, token: &str, from: &str, to: &str) -> Result<Agenda, String> {
    let url = agenda_url(base, from, to)?;
    let token = token.trim();
    let request = agent
        .get(url.as_str())
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {token}"));
    let response = request.call().map_err(|error| match error {
        ureq::Error::Status(401, _) => "令牌无效，请在设置里重新填写".to_string(),
        ureq::Error::Status(403, _) => "服务器拒绝访问，请检查令牌".to_string(),
        ureq::Error::Status(404, _) => "服务器上没有这个接口，请确认版本".to_string(),
        ureq::Error::Status(503, _) => "服务器未配置访问令牌".to_string(),
        ureq::Error::Status(code, _) => format!("服务返回 {code}"),
        _ => "网络不可用".to_string(),
    })?;
    let mut body = String::new();
    if let Err(error) = response
        .into_reader()
        .read_to_string(&mut body)
    {
        return Err(format!("读取课表响应失败：{error}"));
    }
    if body.len() > 4 * 1024 * 1024 {
        return Err("课表响应过大".into());
    }
    parse(&body, chrono::Local::now().timestamp())
}

/// 构造 HTTP 客户端。代理设置与天气模块一致（跟随系统代理）。
pub fn http_agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new().timeout(Duration::from_secs(12));
    let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .or_else(crate::quota::system_proxy);
    if let Some(proxy) = proxy.and_then(|value| ureq::Proxy::new(value).ok()) {
        builder = builder.proxy(proxy);
    }
    builder.build()
}

// ---------------------------------------------------------------- 状态

/// 课表模块的运行时状态：当前快照、错误、缓存。
#[derive(Default)]
pub struct State {
    pub agenda: Option<Agenda>,
    pub error: Option<String>,
    pub loading: bool,
    /// 最近一次发起请求的时刻（秒）。
    pub last_start: i64,
    /// 连续失败次数；自动刷新按 2、4、8、16、30 分钟退避。
    pub failures: u32,
    /// 缓存文件路径。
    pub cache: Option<PathBuf>,
}

impl State {
    /// 载入缓存（离线时至少能显示上一次的结果）。
    pub fn load() -> Self {
        let dir = crate::model::config_dir();
        let path = dir.join("nos-agenda.json");
        let agenda = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Agenda>(&raw).ok());
        Self {
            agenda,
            cache: Some(path),
            ..Default::default()
        }
    }

    /// 把快照写回缓存。
    pub fn persist(&self) {
        if let (Some(path), Some(agenda)) = (&self.cache, &self.agenda) {
            if let Ok(raw) = serde_json::to_vec(agenda) {
                let _ = crate::model::atomic_write(path, &raw);
            }
        }
    }

    /// 失败退避间隔（秒）：断网时不再频繁重试。
    pub fn backoff(&self) -> f64 {
        if self.failures == 0 {
            0.0
        } else {
            (BACKOFF_BASE * 2f64.powi(self.failures.min(5) as i32 - 1)).min(MAX_BACKOFF)
        }
    }

    /// 是否该刷新：从未拉过、配置变化、到达间隔，或上次失败已过退避时间。
    pub fn should_refresh(&self, t: i64) -> bool {
        if self.loading {
            return false;
        }
        if self.error.is_some() {
            return t - self.last_start >= self.backoff() as i64;
        }
        self.last_start == 0 || t - self.last_start >= INTERVAL
    }
}

// ---------------------------------------------------------------- 凭据

/// 生成 32 字节随机十六进制令牌（64 字符），供本地服务模式自动配置使用。
fn generate_token() -> Result<String, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes)
        .map_err(|_| "无法生成随机令牌".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 凭据只进 Windows 凭据管理器；配置文件里只有服务器地址，不落盘令牌。
#[cfg(windows)]
pub mod secrets {
    /// 凭据管理器里的服务名。
    const SERVICE: &str = "Daozai.Nos";

    pub fn save(token: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, "token")
            .map_err(|e| format!("凭据管理器不可用：{e}"))?;
        entry.set_password(token).map_err(|e| format!("保存令牌失败：{e}"))
    }

    pub fn load() -> Result<String, String> {
        let entry = keyring::Entry::new(SERVICE, "token")
            .map_err(|e| format!("凭据管理器不可用：{e}"))?;
        entry.get_password().map_err(|_| "没有保存令牌".to_string())
    }

    /// 本地服务模式下令牌只是岛仔与本机服务之间的共享密钥，没有理由让用户手工生成。
    /// 已有令牌就复用，否则生成 32 字节随机 hex 并保存，服务端和客户端从此自动对齐。
    pub fn ensure_token() -> Result<String, String> {
        if let Ok(existing) = load() {
            if !existing.trim().is_empty() { return Ok(existing); }
        }
        let token = super::generate_token()?;
        save(&token)?;
        Ok(token)
    }

    pub fn delete() {
        if let Ok(entry) = keyring::Entry::new(SERVICE, "token") {
            let _ = entry.delete_credential();
        }
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
    pub fn ensure_token() -> Result<String, String> {
        let token = super::generate_token()?;
        let _ = save(&token);
        Ok(token)
    }
    pub fn delete() {}
}

// ---------------------------------------------------------------- 后台服务

/// 课表配置：服务器地址 + 令牌（令牌只进凭据管理器，不落盘配置文件）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// 服务器地址，如 `http://192.168.1.10:8000`。
    pub base_url: String,
    /// Bearer 令牌（不含 "Bearer " 前缀）。
    pub token: String,
}

impl Config {
    /// 配置是否可用：地址与令牌都要有。
    pub fn ready(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.token.trim().is_empty()
    }
}

/// 后台拉取服务。丢弃即停线程。
pub struct Service {
    rx: Receiver<Result<Agenda, String>>,
    stop: Arc<AtomicBool>,
}

impl Service {
    pub fn start(ctx: egui::Context, cfg: Config) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let _ = crate::resources::NOS.spawn(move || run(ctx, cfg, tx, flag));
        Self { rx, stop }
    }

    pub fn try_recv(&self) -> Option<Result<Agenda, String>> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 后台线程：按间隔拉一次课表，失败时退避。
///
/// 配置变更由外壳重启本服务实现（`Cmd::NosReconfigure`），与系统监控同一策略，
/// 因此线程内不再处理配置切换。
fn run(ctx: egui::Context, cfg: Config, tx: SyncSender<Result<Agenda, String>>, stop: Arc<AtomicBool>) {
    let agent = http_agent();
    let mut clock = PollClock::default();
    let mut failures: u32 = 0;

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

        let today = chrono::Local::now().date_naive();
        let from = today.format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(WINDOW_DAYS))
            .format("%Y-%m-%d")
            .to_string();

        let result = fetch(&agent, &cfg.base_url, &cfg.token, &from, &to);
        match &result {
            Ok(agenda) => {
                failures = 0;
                // 缓存落盘：托盘「退出」是 ExitProcess，后台线程不走 Drop，
                // 因此这里显式写盘。
                let _ = crate::model::atomic_write(
                    &crate::model::config_dir().join("nos-agenda.json"),
                    &serde_json::to_vec(agenda).unwrap_or_default(),
                );
            }
            Err(_) => {
                failures = failures.saturating_add(1);
            }
        }
        let _ = tx.try_send(result);
        ctx.request_repaint();

        // 成功后按正常间隔等待；失败则退避。
        let wait = if failures == 0 {
            INTERVAL
        } else {
            (BACKOFF_BASE * 2f64.powi(failures.min(5) as i32 - 1)).min(MAX_BACKOFF) as i64
        };
        let deadline = Instant::now() + Duration::from_secs(wait as u64);
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 测试连接：拉一次并返回结果与学期信息。
pub fn test_connection(agent: &ureq::Agent, base_url: &str, token: &str) -> Result<String, String> {
    let today = chrono::Local::now().date_naive();
    let from = today.format("%Y-%m-%d").to_string();
    let to = today.format("%Y-%m-%d").to_string();
    let agenda = fetch(agent, base_url, token, &from, &to)?;
    let courses = agenda.events.iter().filter(|e| e.is_course()).count();
    let semester = agenda
        .current_semester()
        .map(|s| s.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "未设置学期".to_string());
    Ok(format!("连接成功 · {semester} · 今天 {courses} 节课"))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_is_64_lowercase_hex_and_unique() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert_eq!(first.len(), 64, "32 字节应为 64 个十六进制字符");
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_ne!(first, second, "两次自动生成不能得到同一个令牌");
    }

    fn sample() -> &'static str {
        r##"{
          "generated_at": "2026-11-14T10:00:00",
          "timezone": "local",
          "semesters": [{"id":"s1","name":"2026 春季","week_start_day":"monday"}],
          "events": [
            {"uid":"nos-course-c1-2026-11-17-1","kind":"course","title":"信号与系统",
             "date":"2026-11-17","start_time":"08:00","end_time":"09:40","all_day":false,
             "location":"教三 402","color":"#A8C0B8","teacher":"张三",
             "course_id":"c1","semester_id":"s1","week":11},
            {"uid":"nos-course-c2-2026-11-18-3","kind":"course","title":"数字逻辑",
             "date":"2026-11-18","start_time":"09:50","end_time":"10:35","all_day":false,
             "location":"实验楼 5","color":"#B8C0D8","teacher":"李四",
             "course_id":"c2","semester_id":"s1","week":11},
            {"uid":"nos-meeting-m1-2026-11-17","kind":"meeting","title":"班会",
             "date":"2026-11-17","start_time":"19:00","end_time":"20:00","all_day":false,
             "location":"学生活动中心","color":"#D8C0B8","teacher":"",
             "course_id":"","semester_id":"s1","week":11},
            {"uid":"nos-exam-c1","kind":"exam","title":"信号与系统 期末考试",
             "date":"2027-01-15","start_time":"09:00","end_time":"11:00","all_day":false,
             "location":"","color":"#A8C0B8","teacher":"张三",
             "course_id":"c1","semester_id":"s1","week":null},
            {"uid":"nos-meeting-single-x","kind":"meeting","title":"导师约谈",
             "date":"2026-11-14","start_time":"","end_time":"","all_day":true,
             "location":"行政楼 301","color":"#C0B8D8","teacher":"",
             "course_id":"","semester_id":"","week":null}
          ]
        }"##
    }

    #[test]
    fn parse_reads_events_and_semesters() {
        let agenda = parse(sample(), 100).unwrap();
        assert_eq!(agenda.events.len(), 5);
        assert_eq!(agenda.semesters.len(), 1);
        assert_eq!(agenda.semesters[0].name, "2026 春季");
        assert_eq!(agenda.at, 100);
    }

    #[test]
    fn parse_skips_broken_rows_instead_of_failing_whole_payload() {
        let body = r#"{"semesters":[{"id":"","name":"空 id"}],
          "events":[{"uid":"","title":"无 uid","date":"2026-11-17"},
                    {"uid":"u2","title":"坏日期","date":"not-a-date"},
                    {"uid":"u3","title":"","date":"2026-11-17"},
                    {"uid":"u4","title":"好的","date":"2026-11-17","start_time":"08:00"}]}"#;
        let agenda = parse(body, 1).unwrap();
        assert_eq!(agenda.events.len(), 1, "只保留完整的一条：{:?}", agenda.events);
        assert!(agenda.semesters.is_empty(), "空 id 的学期应被丢弃");
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse("not json", 0).is_err());
    }

    #[test]
    fn wall_clock_uses_local_timezone_and_rejects_ambiguous_or_invalid() {
        let value = wall_clock("2026-11-17", "08:00").expect("普通时刻应能换算");
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 11, 17)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap();
        let expect = naive
            .and_local_timezone(chrono::Local)
            .single()
            .map(|dt| dt.timestamp())
            .unwrap();
        assert_eq!(value, expect, "必须按本机本地时区解释");
        assert_eq!(wall_clock("2026-11-17", ""), None, "空时刻没有意义");
        assert_eq!(wall_clock("bad-date", "08:00"), None);
        assert_eq!(wall_clock("2026-11-17", "25:00"), None);
    }

    #[test]
    fn next_course_picks_the_earliest_future_class_and_skips_meetings() {
        let agenda = parse(sample(), 0).unwrap();
        // 取课程 A 开始时刻之前 1 秒 → 下一节应是「信号与系统」
        let start = agenda.events[0].start_at().unwrap();
        let next = agenda.next_course(start - 1).expect("应有下一节课");
        assert_eq!(next.title, "信号与系统");
        assert!(next.is_course());
        // 过了所有课程时间 → 没有下一节课
        let late = agenda.events[1].end_at().unwrap() + 1;
        assert!(agenda.next_course(late).is_none(), "所有课都结束后不应再有下一节");
    }

    #[test]
    fn current_course_detects_the_class_in_progress() {
        let agenda = parse(sample(), 0).unwrap();
        let start = agenda.events[0].start_at().unwrap();
        let end = agenda.events[0].end_at().unwrap();
        let current = agenda.current_course(start + 10).expect("课中应能识别当前课程");
        assert_eq!(current.title, "信号与系统");
        assert!(agenda.current_course(end + 10).is_none(), "课后不应还算在上课");
    }

    #[test]
    fn today_courses_excludes_meetings_and_exams() {
        let agenda = parse(sample(), 0).unwrap();
        let courses = agenda.today_courses("2026-11-17");
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].title, "信号与系统");
        assert!(agenda.today_courses("2026-11-14").is_empty(), "当天只有一场全天会议");
    }

    #[test]
    fn day_lists_all_day_events_first() {
        let agenda = parse(sample(), 0).unwrap();
        let day = agenda.day("2026-11-14");
        assert_eq!(day.len(), 1);
        assert!(day[0].all_day, "全天事件应排在最前");
    }

    #[test]
    fn place_falls_back_to_teacher_then_empty() {
        let agenda = parse(sample(), 0).unwrap();
        let course = agenda.events.iter().find(|e| e.is_course()).unwrap();
        assert_eq!(course.place(), "教三 402", "有地点时用地点");
        let exam = agenda.events.iter().find(|e| e.is_exam()).unwrap();
        assert_eq!(exam.place(), "张三", "没地点时回退到教师");
    }

    #[test]
    fn clock_label_normalizes_to_two_digits() {
        assert_eq!(clock_label("8:5"), "08:05");
        assert_eq!(clock_label(""), "");
        assert_eq!(clock_label("25:00"), "25:00", "非法值原样返回");
    }

    #[test]
    fn agenda_url_appends_date_range() {
        let url = agenda_url("http://example.com:8000/", "2026-11-14", "2026-11-20").unwrap();
        assert_eq!(url.path(), "/api/daozai/agenda", "必须打到聚合接口，不能打到静态首页");
        assert!(url.as_str().contains("from=2026-11-14"), "{}", url);
        assert!(url.as_str().contains("to=2026-11-20"), "{}", url);
        let with_path = agenda_url("http://example.com:8000/nos", "a", "b").unwrap();
        assert_eq!(with_path.path(), "/api/daozai/agenda", "填了子路径也要落到固定接口");
        assert!(agenda_url("not a url", "a", "b").is_err());
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let mut state = State::default();
        assert_eq!(state.backoff(), 0.0);
        state.failures = 1;
        assert_eq!(state.backoff(), 120.0);
        state.failures = 2;
        assert_eq!(state.backoff(), 240.0);
        state.failures = 9;
        assert_eq!(state.backoff(), MAX_BACKOFF, "退避必须有上限");
    }

    #[test]
    fn should_refresh_respects_interval_and_backoff() {
        let mut state = State::default();
        assert!(state.should_refresh(0), "从未拉过应立即刷新");
        state.last_start = 1000;
        state.loading = false;
        assert!(!state.should_refresh(1000 + INTERVAL - 1), "间隔未到不刷新");
        assert!(state.should_refresh(1000 + INTERVAL), "间隔到了应刷新");
        state.error = Some("网络不可用".into());
        state.failures = 1;
        assert!(!state.should_refresh(1000 + 60), "失败后退避期（120 秒）内不刷新");
        assert!(state.should_refresh(1000 + 120), "退避结束应重试");
        state.loading = true;
        assert!(!state.should_refresh(1000 + 120), "在途请求不重复发起");
    }
}
