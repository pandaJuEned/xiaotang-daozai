//! 手动记录的「限流到点提醒」。
//!
//! WorkBuddy 这类服务在模型免费额度被限频时，会在界面上给出一个精确的恢复时刻
//! （「可在 2026-11-17 23:43:31 重置后继续使用」），但这个时刻只活在那一条提示里：
//! 积分接口只返回余量、不返回周期，第三方查不到。
//!
//! 所以做法不去逆向接口，而是「看到提示手动记一笔，到点由岛仔叫一声」：
//! 记下来的是账号 + 模型 + 恢复时刻，过了时刻就提醒一次，随后自动退场。
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 到点之后再多留半小时，卡片上还能看到「已到时间」，之后自动消失。
const STALE_SECS: i64 = 30 * 60;
/// 剪贴板里最多扫这么多字符：整份日志没必要逐字找时间。
const SCAN_LIMIT: usize = 4000;

/// 一条限流记录：哪个账号、哪个模型、什么时候恢复。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub account: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub model: String,
    /// 恢复时刻（本地时区对应的 Unix 秒）。
    pub reset_at: i64,
    #[serde(default)]
    pub created_at: i64,
    /// 已经提醒过，不再重复响。
    #[serde(default)]
    pub notified: bool,
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub items: Vec<Limit>,
}

impl Limits {
    pub fn path() -> PathBuf {
        crate::model::config_dir().join("quota-limits.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Limits>(&raw).ok())
            .map(|mut limits| {
                // 关机期间早就到点的记录：现在再喊一句「可以用了」没意义，直接丢掉。
                let now = chrono::Local::now().timestamp();
                limits.items.retain(|item| item.reset_at > now - STALE_SECS);
                limits
            })
            .unwrap_or_default()
    }

    fn save_to(&self, path: &Path) {
        let Ok(bytes) = serde_json::to_vec(self) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::model::atomic_write(path, &bytes);
    }

    /// 同一账号同一模型只留最新一条；旧记录到点没提醒的，顺手作废。
    pub fn upsert(&mut self, limit: Limit) {
        self.items.retain(|item| item.account != limit.account || item.model != limit.model);
        self.items.push(limit);
        self.items.sort_by_key(|item| item.reset_at);
    }

    /// 这个账号当前还没恢复的那条（有多个时取最早到点的）。
    pub fn active(&self, account: &str, now: i64) -> Option<&Limit> {
        self.items
            .iter()
            .filter(|item| item.account == account && item.reset_at > now)
            .min_by_key(|item| item.reset_at)
    }

    pub fn clear(&mut self, account: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.account != account);
        self.items.len() != before
    }

    /// 取走到点的记录（每条只取一次），同时清掉早已过期的。
    /// 返回 `(本次到点, 是否改动过)`。
    pub fn poll(&mut self, now: i64) -> (Vec<Limit>, bool) {
        let mut due = Vec::new();
        for item in &mut self.items {
            if !item.notified && item.reset_at <= now {
                item.notified = true;
                due.push(item.clone());
            }
        }
        let before = self.items.len();
        self.items.retain(|item| !(item.notified && item.reset_at + STALE_SECS <= now));
        let changed = !due.is_empty() || self.items.len() != before;
        (due, changed)
    }
}

/// 「还有 1 小时 22 分」这类相对时间；到点后给「已到时间」。
pub fn remaining_text(reset_at: i64, now: i64) -> String {
    let left = reset_at - now;
    if left <= 0 {
        return "已到时间".into();
    }
    let minutes = (left + 59) / 60;
    if minutes < 60 {
        format!("还有 {minutes} 分钟")
    } else {
        let (hours, minutes) = (minutes / 60, minutes % 60);
        if minutes == 0 {
            format!("还有 {hours} 小时")
        } else {
            format!("还有 {hours} 小时 {minutes} 分")
        }
    }
}

/// 恢复时刻的短写法：当天只给 `23:43`，跨天补上 `11/18 09:10`。
pub fn when_text(reset_at: i64) -> String {
    let Some(at) = chrono::DateTime::from_timestamp(reset_at, 0) else { return String::new() };
    let at = at.with_timezone(&chrono::Local);
    let pattern = if at.date_naive() == chrono::Local::now().date_naive() { "%H:%M" } else { "%m/%d %H:%M" };
    at.format(pattern).to_string()
}

/// 完整写法，用于回填输入框：`2026-11-17 23:43:31`。
pub fn full_text(reset_at: i64) -> String {
    chrono::DateTime::from_timestamp(reset_at, 0)
        .map(|at| at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// 把用户填的时刻折算成 Unix 秒。
///
/// 只填 `23:43:31` 时按今天算；今天这个点已经过了就顺延到明天（界面上会显示折算结果，
/// 不会悄悄跑到意外的日子）。带日期的写法一律按填的日期算。
pub fn parse_reset(input: &str, now: chrono::DateTime<chrono::Local>) -> Result<i64, String> {
    let text = input.trim().replace(['：', '／'], ":").replace('/', "-");
    if text.is_empty() {
        return Err("先填一个恢复时刻".into());
    }
    let parts = text.split_whitespace().collect::<Vec<_>>();
    let (date, time) = match parts.as_slice() {
        [time] if time.contains(':') => (None, *time),
        [date, time] => (Some(*date), *time),
        _ => return Err("填成 23:43，或 2026-11-17 23:43:31".into()),
    };
    let time = parse_time(time)?;
    let naive = match date {
        Some(date) => parse_date(date)?.and_time(time),
        None => {
            let today = now.date_naive().and_time(time);
            if today > now.naive_local() {
                today
            } else {
                match now.date_naive().succ_opt() {
                    Some(next) => next.and_time(time),
                    None => return Err("日期超出范围".into()),
                }
            }
        }
    };
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|at| at.timestamp())
        .ok_or_else(|| "这个时刻在本地时区里不存在".into())
}

fn parse_date(text: &str) -> Result<chrono::NaiveDate, String> {
    let fields = text.split('-').collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err("日期写成 2026-11-17".into());
    }
    let number = |value: &str| value.parse::<i64>().map_err(|_| "日期里有看不懂的数字".to_string());
    let (mut year, month, day) = (number(fields[0])?, number(fields[1])?, number(fields[2])?);
    if year < 100 {
        year += 2000;
    }
    let (year, month, day) = (year.try_into().unwrap_or(0), month as u32, day as u32);
    chrono::NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| "这个日期不存在".into())
}

fn parse_time(text: &str) -> Result<chrono::NaiveTime, String> {
    let fields = text.split(':').collect::<Vec<_>>();
    if fields.len() < 2 || fields.len() > 3 {
        return Err("时刻写成 23:43 或 23:43:31".into());
    }
    let numbers = fields
        .iter()
        .map(|value| value.parse::<u32>().map_err(|_| "时刻里有看不懂的数字".to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let second = numbers.get(2).copied().unwrap_or(0);
    chrono::NaiveTime::from_hms_opt(numbers[0], numbers[1], second).ok_or_else(|| "这个时刻不存在".into())
}

/// 从一段文字里挑出最像恢复时刻的那一小段（复制 WorkBuddy 的提示就能自动填上）。
pub fn find_when(text: &str, now: chrono::DateTime<chrono::Local>) -> Option<String> {
    let chars = text.chars().take(SCAN_LIMIT).collect::<Vec<_>>();
    let mut clock = None;
    for start in 0..chars.len() {
        let Some((raw, dated)) = match_at(&chars[start..]) else { continue };
        if parse_reset(&raw, now).is_err() {
            continue;
        }
        if dated {
            return Some(raw);
        }
        if clock.is_none() {
            clock = Some(raw);
        }
    }
    clock
}

/// 从某个位置起尝试读出一段时间；返回 `(原文, 是否带日期)`。
fn match_at(chars: &[char]) -> Option<(String, bool)> {
    // 带日期：2026-11-17 23:43:31 / 2026-11-17 23:43
    if let Some((_, after_year)) = digits(chars, 0, 4, 4) {
        if chars.get(after_year) == Some(&'-') {
            if let Some((_, after_month)) = digits(chars, after_year + 1, 1, 2) {
                if chars.get(after_month) == Some(&'-') {
                    if let Some((_, after_day)) = digits(chars, after_month + 1, 1, 2) {
                        let mut at = after_day;
                        if chars.get(at).is_some_and(|ch| *ch == ' ') {
                            at += 1;
                        }
                        if let Some((_, after_hour)) = digits(chars, at, 1, 2) {
                            if chars.get(after_hour) == Some(&':') {
                                if let Some((_, after_minute)) = digits(chars, after_hour + 1, 2, 2) {
                                    let mut end = after_minute;
                                    if chars.get(end) == Some(&':') {
                                        if let Some((_, after_second)) = digits(chars, end + 1, 2, 2) {
                                            end = after_second;
                                        }
                                    }
                                    return Some((chars[..end].iter().collect(), true));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // 只有时刻：23:43:31 / 23:43
    if let Some((_, after_hour)) = digits(chars, 0, 1, 2) {
        if chars.get(after_hour) == Some(&':') {
            if let Some((_, after_minute)) = digits(chars, after_hour + 1, 2, 2) {
                let mut end = after_minute;
                if chars.get(end) == Some(&':') {
                    if let Some((_, after_second)) = digits(chars, end + 1, 2, 2) {
                        end = after_second;
                    }
                }
                // 后面紧跟数字说明是从更长的串里截出来的，不算数。
                if chars.get(end).is_none_or(|ch| !ch.is_ascii_digit()) {
                    return Some((chars[..end].iter().collect(), false));
                }
            }
        }
    }
    None
}

fn digits(chars: &[char], at: usize, min: usize, max: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    let mut taken = 0;
    while taken < max {
        match chars.get(at + taken).and_then(|ch| ch.to_digit(10)) {
            Some(digit) => {
                value = value * 10 + digit;
                taken += 1;
            }
            None => break,
        }
    }
    if taken < min { None } else { Some((value, at + taken)) }
}

/// 界面里那一个小窗的草稿。
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub account: String,
    pub alias: String,
    pub model: String,
    pub when: String,
    pub focused: bool,
}

impl Draft {
    pub fn new(account: &str, alias: &str, model: &str, when: &str) -> Self {
        Self {
            account: account.into(),
            alias: alias.into(),
            model: model.into(),
            when: when.into(),
            ..Default::default()
        }
    }

    /// 折算成待保存的记录；时刻填得不对时把错误留在草稿上。
    pub fn build(&self, now: chrono::DateTime<chrono::Local>) -> Result<Limit, String> {
        let reset_at = parse_reset(&self.when, now)?;
        // 早在一分钟前就过去的时刻多半是日期写错了，别让它一保存就立刻响。
        if reset_at < now.timestamp() - 60 {
            return Err("这个时刻已经过了，确认一下日期".into());
        }
        Ok(Limit {
            account: self.account.clone(),
            alias: self.alias.clone(),
            model: self.model.trim().to_string(),
            reset_at,
            created_at: now.timestamp(),
            notified: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32, minute: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 11, 17, hour, minute, 0).single().unwrap()
    }

    #[test]
    fn clock_only_rolls_to_tomorrow_when_already_past() {
        let now = at(19, 14);
        let evening = parse_reset("23:43:31", now).unwrap();
        assert_eq!(evening, at(23, 43).timestamp() + 31);
        // 已经过去的时刻按明天算，而不是立刻触发。
        let morning = parse_reset("08:00", now).unwrap();
        assert_eq!(morning, chrono::Local.with_ymd_and_hms(2026, 11, 18, 8, 0, 0).single().unwrap().timestamp());
    }

    #[test]
    fn dated_forms_keep_the_given_day() {
        let now = at(19, 14);
        let expected = at(23, 43).timestamp() + 31;
        for text in ["2026-11-17 23:43:31", "2026/11/17 23:43:31", "2026-11-17   23:43:31", " 2026-11-17 23:43:31 "] {
            assert_eq!(parse_reset(text, now).unwrap(), expected, "{text}");
        }
        assert_eq!(parse_reset("2026-11-17 23:43", now).unwrap(), at(23, 43).timestamp());
        // 跨天记录也是原样保留。
        assert_eq!(parse_reset("2026-11-18 09:10", now).unwrap(), chrono::Local.with_ymd_and_hms(2026, 11, 18, 9, 10, 0).single().unwrap().timestamp());
    }

    #[test]
    fn rejects_unreadable_input() {
        let now = at(19, 14);
        for text in ["", "   ", "明天", "23:43:31:01", "25:00", "2026-13-01 09:00", "2026-11-17"] {
            assert!(parse_reset(text, now).is_err(), "应拒绝：{text}");
        }
    }

    #[test]
    fn clipboard_text_yields_the_reset_moment() {
        let now = at(19, 14);
        let banner = "您使用 Hy4 flash 模型的免费额度已超出频率限制，可在 2026-11-17 23:43:31 重置后继续使用。您可以消耗积分继续使用该模型，或切换至其他模型。";
        assert_eq!(find_when(banner, now).as_deref(), Some("2026-11-17 23:43:31"));
        assert_eq!(find_when("额度超限，23:59 恢复", now).as_deref(), Some("23:59"));
        assert!(find_when("没有任何时间的一段话", now).is_none());
    }

    #[test]
    fn remaining_text_stays_readable() {
        let now = 1_000_000;
        assert_eq!(remaining_text(now + 30, now), "还有 1 分钟");
        assert_eq!(remaining_text(now + 59 * 60, now), "还有 59 分钟");
        assert_eq!(remaining_text(now + 3600, now), "还有 1 小时");
        assert_eq!(remaining_text(now + 3600 + 22 * 60, now), "还有 1 小时 22 分");
        assert_eq!(remaining_text(now - 1, now), "已到时间");
    }

    #[test]
    fn due_fires_once_then_retires() {
        let mut limits = Limits::default();
        limits.upsert(Limit { account: "a".into(), model: "Hy4 flash".into(), reset_at: 1_000, ..Default::default() });
        // 同一账号同一模型重复记录：只留最新一条。
        limits.upsert(Limit { account: "a".into(), model: "Hy4 flash".into(), reset_at: 2_000, ..Default::default() });
        assert_eq!(limits.items.len(), 1);
        limits.upsert(Limit { account: "a".into(), model: "另一个模型".into(), reset_at: 1_500, ..Default::default() });
        assert_eq!(limits.items.len(), 2);

        assert!(limits.active("a", 900).is_some());
        let (due, changed) = limits.poll(1_600);
        assert!(changed);
        assert_eq!(due.len(), 1, "只该提醒到期的那一条");
        assert_eq!(due[0].model, "另一个模型");
        // 再轮询不会重复响。
        assert!(limits.poll(1_600).0.is_empty());
        // 过期半小时后自动退场；未到点的记录不受影响。
        let (due, changed) = limits.poll(2_000 + STALE_SECS + 1);
        assert!(changed);
        assert_eq!(due.len(), 1);
        assert!(limits.items.is_empty());
    }

    #[test]
    fn clear_drops_only_that_account() {
        let mut limits = Limits::default();
        limits.upsert(Limit { account: "a".into(), model: "m".into(), reset_at: 10, ..Default::default() });
        limits.upsert(Limit { account: "b".into(), model: "m".into(), reset_at: 10, ..Default::default() });
        assert!(limits.clear("a"));
        assert_eq!(limits.items.len(), 1);
        assert_eq!(limits.items[0].account, "b");
        assert!(!limits.clear("c"));
    }

    #[test]
    fn draft_saves_and_reloads() {
        let dir = std::env::temp_dir().join(format!("daozai-limits-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("quota-limits.json");

        let now = at(19, 14);
        let now = at(19, 14);
        let draft = Draft::new("acc", "WorkBuddy", " Hy4 flash ", "23:43:31");
        let mut limit = draft.build(now).unwrap();
        assert_eq!(limit.model, "Hy4 flash", "模型名两头空格要修掉");
        assert_eq!(limit.alias, "WorkBuddy");
        assert_eq!(limit.reset_at, at(23, 43).timestamp() + 31);

        // 落盘再用真实时钟读回来：换成一定还没到点的时刻，测试才不会依赖跑在几点。
        limit.reset_at = chrono::Local::now().timestamp() + 3600;
        let mut limits = Limits::default();
        limits.upsert(limit.clone());
        limits.save_to(&path);
        let reloaded = Limits::load_from(&path);
        assert_eq!(reloaded.items, vec![limit.clone()]);

        // 草稿里时间写错时不产生记录。
        assert!(Draft::new("acc", "WorkBuddy", "m", "23:43:31:01").build(now).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_records_do_not_return_after_restart() {
        let dir = std::env::temp_dir().join(format!("daozai-limits-stale-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("quota-limits.json");
        let old = chrono::Local::now().timestamp() - STALE_SECS - 60;
        std::fs::write(&path, format!("{{\"items\":[{{\"account\":\"a\",\"model\":\"m\",\"reset_at\":{old}}}]}}")).unwrap();
        assert!(Limits::load_from(&path).items.is_empty(), "关机期间早就到点的记录重开时不该再补一句提醒");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn draft_refuses_a_moment_that_already_passed() {
        let now = at(19, 14);
        assert!(Draft::new("a", "b", "", "2026-11-17 09:00").build(now).is_err(), "一小时前的时刻不该悄悄存进去");
        // 一分钟内的容差：刚刚过点也允许记（用户就坐在提示前）。
        assert!(Draft::new("a", "b", "", "2026-11-17 19:13").build(now).is_ok());
    }
}
