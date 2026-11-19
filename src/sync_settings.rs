//! 账号级设置同步（P5）：一个键一条 `setting-<key>` 记录。
//!
//! 只同步「换一台机器也应当一致」的项；窗口位置、显示器、置物架路径、通知监听、
//! 邮件账号与各类凭据都留在本机（见方案 §7.5）。一个键一条记录，冲突只影响这一项，
//! 不会像整表同步那样把别的设置一起覆盖。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::app::App;
use crate::model::Settings;
use crate::sync::{self, Ack, Op, Record};

fn meta_path() -> PathBuf {
    crate::model::config_dir().join("settings-sync.json")
}

/// 账本级白名单。新增键时同步改 `get` / `set` 与方案 §7.5。
pub const KEYS: &[&str] = &[
    "skin",
    "island_shape",
    "glass_blur",
    "settings_theme",
    "pet",
    "minimal",
    "clock_12h",
    "clock_period",
    "visible_pages",
    "page_order",
    "resident_order",
    "notifications",
    "sound",
    "notification_preview",
    "system_hud",
    "bluetooth_notifications",
    "lyrics_enabled",
    "island_lyrics",
    "lyric_delay_ms",
    "focus_minutes",
    "rest_minutes",
    "water_minutes",
    "serving_ml",
    "water_start",
    "water_end",
    "sit_remind",
    "sit_minutes",
    "lunch_rest",
    "lunch_start",
    "lunch_end",
    "mail_code",
    "mail_code_hide_seconds",
    "calendar_prompt",
    "calendar_auto_sync",
    // 天气城市 / 定位 / 缓存是机器级：每台看自己所在地的天气；
    // 其他设备的天气由 presence 心跳带过来，显示为「异地天气」。
    "quota_reset_notice",
];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Entry {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    pending: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Meta {
    #[serde(default)]
    items: BTreeMap<String, Entry>,
}

impl Meta {
    fn load() -> Self {
        std::fs::read(meta_path())
            .ok()
            .and_then(|raw| serde_json::from_slice::<Meta>(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(raw) = serde_json::to_vec(self) {
            let _ = crate::model::atomic_write(&meta_path(), &raw);
        }
    }
}

fn hash_value(value: &Value) -> String {
    let raw = serde_json::to_vec(value).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------- 取值

pub fn get(settings: &Settings, key: &str) -> Option<Value> {
    Some(match key {
        "skin" => Value::String(settings.skin.key().into()),
        "island_shape" => Value::String(settings.island_shape.key().into()),
        "glass_blur" => Value::Bool(settings.glass_blur),
        "settings_theme" => Value::String(settings.settings_theme.key().into()),
        "pet" => Value::Bool(settings.pet),
        "minimal" => Value::Bool(settings.minimal),
        "clock_12h" => Value::Bool(settings.clock_12h),
        "clock_period" => Value::from(settings.clock_period),
        "visible_pages" => serde_json::to_value(settings.visible_pages).ok()?,
        "page_order" => serde_json::to_value(settings.page_order).ok()?,
        "resident_order" => serde_json::to_value(&settings.resident_order).ok()?,
        "notifications" => Value::Bool(settings.notifications),
        "sound" => Value::Bool(settings.sound),
        "notification_preview" => Value::Bool(settings.notification_preview),
        "system_hud" => Value::Bool(settings.system_hud),
        "bluetooth_notifications" => Value::Bool(settings.bluetooth_notifications),
        "lyrics_enabled" => Value::Bool(settings.lyrics_enabled),
        "island_lyrics" => Value::Bool(settings.island_lyrics),
        "lyric_delay_ms" => Value::from(settings.lyric_delay_ms),
        "focus_minutes" => Value::from(settings.focus_minutes),
        "rest_minutes" => Value::from(settings.rest_minutes),
        "water_minutes" => Value::from(settings.water_minutes),
        "serving_ml" => Value::from(settings.serving_ml),
        "water_start" => Value::from(settings.water_start),
        "water_end" => Value::from(settings.water_end),
        "sit_remind" => Value::Bool(settings.sit_remind),
        "sit_minutes" => Value::from(settings.sit_minutes),
        "lunch_rest" => Value::Bool(settings.lunch_rest),
        "lunch_start" => Value::from(settings.lunch_start),
        "lunch_end" => Value::from(settings.lunch_end),
        "mail_code" => Value::Bool(settings.mail_code),
        "mail_code_hide_seconds" => Value::from(settings.mail_code_hide_seconds),
        "calendar_prompt" => Value::Bool(settings.calendar_prompt),
        "calendar_auto_sync" => Value::Bool(settings.calendar_auto_sync),
        "weather_city" => Value::String(settings.weather_city.clone()),
        "weather_position" => serde_json::to_value(&settings.weather_position).ok()?,
        "weather_saved" => serde_json::to_value(&settings.weather_saved).ok()?,
        "quota_reset_notice" => Value::Bool(settings.quota_reset_notice),
        _ => return None,
    })
}

fn set_bool(dst: &mut bool, value: &Value) -> bool {
    let Some(next) = value.as_bool() else {
        return false;
    };
    *dst = next;
    true
}

fn set_u32(dst: &mut u32, value: &Value, lo: u32, hi: u32) -> bool {
    let Some(next) = value.as_u64().map(|number| number as u32) else {
        return false;
    };
    if !(lo..=hi).contains(&next) {
        return false;
    }
    *dst = next;
    true
}

fn set_u8(dst: &mut u8, value: &Value, lo: u8, hi: u8) -> bool {
    let Some(next) = value.as_u64().map(|number| number as u8) else {
        return false;
    };
    if !(lo..=hi).contains(&next) {
        return false;
    }
    *dst = next;
    true
}

fn set_i32(dst: &mut i32, value: &Value, lo: i32, hi: i32) -> bool {
    let Some(next) = value.as_i64().map(|number| number as i32) else {
        return false;
    };
    if !(lo..=hi).contains(&next) {
        return false;
    }
    *dst = next;
    true
}

// ---------------------------------------------------------------- 赋值

pub fn set(settings: &mut Settings, key: &str, value: &Value) -> bool {
    match key {
        "skin" => {
            let Some(text) = value.as_str() else { return false };
            settings.skin = crate::model::Skin::parse(text);
            true
        }
        "island_shape" => {
            let Some(text) = value.as_str() else { return false };
            settings.island_shape = crate::model::IslandShape::parse(text);
            true
        }
        "glass_blur" => set_bool(&mut settings.glass_blur, value),
        "settings_theme" => {
            let Some(text) = value.as_str() else { return false };
            settings.settings_theme = crate::settings_theme::SettingsTheme::parse(text);
            true
        }
        "pet" => set_bool(&mut settings.pet, value),
        "minimal" => {
            if !set_bool(&mut settings.minimal, value) {
                return false;
            }
            // 与设置页一致：极简模式会关掉吉祥物。
            if settings.minimal {
                settings.pet = false;
            }
            true
        }
        "clock_12h" => set_bool(&mut settings.clock_12h, value),
        "clock_period" => set_u8(&mut settings.clock_period, value, 0, 2),
        "visible_pages" => {
            let Some(items) = value.as_array() else { return false };
            if items.len() != 10 {
                return false;
            }
            let mut next = [false; 10];
            for (index, item) in items.iter().enumerate() {
                next[index] = item.as_bool().unwrap_or(false);
            }
            if !next.iter().any(|enabled| *enabled) {
                return false; // 全关会让岛仔没有页面可显示
            }
            settings.visible_pages = next;
            true
        }
        "page_order" => {
            let Some(items) = value.as_array() else { return false };
            if items.len() != 10 {
                return false;
            }
            let mut next = [0usize; 10];
            for (index, item) in items.iter().enumerate() {
                let Some(page) = item.as_u64().map(|number| number as usize) else {
                    return false;
                };
                if page >= 10 || next[..index].contains(&page) {
                    return false;
                }
                next[index] = page;
            }
            settings.page_order = next;
            true
        }
        "resident_order" => {
            let Some(items) = value.as_array() else { return false };
            let mut next = Vec::new();
            for item in items {
                let Some(index) = item.as_u64().map(|number| number as usize) else {
                    return false;
                };
                if index < 16 && !next.contains(&index) {
                    next.push(index);
                }
            }
            settings.resident_order = next;
            true
        }
        "notifications" => set_bool(&mut settings.notifications, value),
        "sound" => set_bool(&mut settings.sound, value),
        "notification_preview" => set_bool(&mut settings.notification_preview, value),
        "system_hud" => set_bool(&mut settings.system_hud, value),
        "bluetooth_notifications" => set_bool(&mut settings.bluetooth_notifications, value),
        "lyrics_enabled" => set_bool(&mut settings.lyrics_enabled, value),
        "island_lyrics" => set_bool(&mut settings.island_lyrics, value),
        "lyric_delay_ms" => set_i32(&mut settings.lyric_delay_ms, value, -5000, 5000),
        "focus_minutes" => set_u32(&mut settings.focus_minutes, value, 1, 180),
        "rest_minutes" => set_u32(&mut settings.rest_minutes, value, 1, 60),
        "water_minutes" => set_u32(&mut settings.water_minutes, value, 1, 240),
        "serving_ml" => set_u32(&mut settings.serving_ml, value, 50, 1000),
        "water_start" => set_u32(&mut settings.water_start, value, 0, 23),
        "water_end" => set_u32(&mut settings.water_end, value, 0, 23),
        "sit_remind" => set_bool(&mut settings.sit_remind, value),
        "sit_minutes" => set_u32(&mut settings.sit_minutes, value, 5, 240),
        "lunch_rest" => set_bool(&mut settings.lunch_rest, value),
        "lunch_start" => set_u32(&mut settings.lunch_start, value, 0, 1439),
        "lunch_end" => set_u32(&mut settings.lunch_end, value, 0, 1439),
        "mail_code" => set_bool(&mut settings.mail_code, value),
        "mail_code_hide_seconds" => {
            set_u32(&mut settings.mail_code_hide_seconds, value, 0, 600)
        }
        "calendar_prompt" => set_bool(&mut settings.calendar_prompt, value),
        "calendar_auto_sync" => set_bool(&mut settings.calendar_auto_sync, value),
        "weather_city" => {
            let Some(text) = value.as_str() else { return false };
            settings.weather_city = text.trim().chars().filter(|ch| !ch.is_control()).take(80).collect();
            true
        }
        "weather_position" => {
            let Ok(position) = serde_json::from_value::<Option<crate::weather::Position>>(value.clone())
            else {
                return false;
            };
            if position.as_ref().is_some_and(|position| !position.valid()) {
                return false;
            }
            settings.weather_position = position;
            true
        }
        "weather_saved" => {
            let Ok(items) = serde_json::from_value::<Vec<crate::weather::Position>>(value.clone())
            else {
                return false;
            };
            settings.weather_saved = items
                .into_iter()
                .filter(crate::weather::Position::valid)
                .take(8)
                .collect();
            true
        }
        "quota_reset_notice" => set_bool(&mut settings.quota_reset_notice, value),
        _ => false,
    }
}

fn apply_acks(meta: &mut Meta, acks: &[Ack]) {
    for ack in acks {
        for entry in meta.items.values_mut() {
            if entry.pending != ack.op_id {
                continue;
            }
            match ack.status.as_str() {
                "applied" | "duplicate" => {
                    entry.rev = ack.rev.unwrap_or(entry.rev.max(1));
                    entry.pending.clear();
                }
                _ => entry.pending.clear(),
            }
        }
    }
}

/// `sync_map` 每 500ms 调一次；返回留在 inbox 的记录（目前只可能是非 setting 类型）。
pub fn sync(app: &mut App, records: &[Record], acks: &[Ack], ready: bool) -> Vec<Record> {
    let mut meta = Meta::load();
    let before = meta.clone();
    apply_acks(&mut meta, acks);

    let mut changed = false;
    let mut failed = Vec::new();
    for record in records {
        if record.kind != "setting" {
            failed.push(record.clone());
            continue;
        }
        let key = record
            .data
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| record.id.strip_prefix("setting-").map(str::to_owned));
        let Some(key) = key else {
            continue;
        };
        if !KEYS.contains(&key.as_str()) {
            continue; // 新版本新增的键：旧版本安静忽略，不阻塞同步
        }
        let value = record.data.get("value").cloned().unwrap_or(Value::Null);
        if !set(&mut app.settings, &key, &value) {
            continue;
        }
        changed = true;
        let entry = meta.items.entry(key).or_default();
        entry.rev = record.rev;
        entry.hash = hash_value(&value);
        entry.pending.clear();
    }
    if changed {
        app.settings.save();
    }

    if ready {
        for key in KEYS {
            let Some(value) = get(&app.settings, key) else {
                continue;
            };
            let hash = hash_value(&value);
            let entry = meta.items.entry((*key).to_string()).or_default();
            if !entry.pending.is_empty() {
                continue;
            }
            if entry.rev == 0 || entry.hash != hash {
                let op = Op::new(
                    "setting",
                    &format!("setting-{key}"),
                    entry.rev,
                    "",
                    serde_json::json!({"key": key, "value": value}),
                );
                if sync::enqueue(&op).is_ok() {
                    entry.pending = op.op_id.clone();
                    entry.hash = hash;
                }
            }
        }
    }

    if meta != before {
        meta.save();
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_whitelisted_key_roundtrips() {
        let source = Settings::default();
        for key in KEYS {
            let value = get(&source, key).unwrap_or_else(|| panic!("{key} 没有 get"));
            let mut target = Settings::default();
            assert!(set(&mut target, key, &value), "{key} 不能 set");
            assert_eq!(get(&target, key), Some(value), "{key} 往返不一致");
        }
    }

    #[test]
    fn page_order_rejects_duplicates_and_out_of_range() {
        let mut settings = Settings::default();
        let duplicate = serde_json::json!([0, 0, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert!(!set(&mut settings, "page_order", &duplicate));
        let out_of_range = serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 10]);
        assert!(!set(&mut settings, "page_order", &out_of_range));
        let valid = serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 0]);
        assert!(set(&mut settings, "page_order", &valid));
        assert_eq!(settings.page_order[0], 1);
    }

    #[test]
    fn all_pages_off_is_rejected() {
        let mut settings = Settings::default();
        let all_off = serde_json::Value::Array(vec![Value::Bool(false); 10]);
        assert!(!set(&mut settings, "visible_pages", &all_off));
    }

    #[test]
    fn minimal_turns_pet_off_like_the_settings_page() {
        let mut settings = Settings::default();
        settings.pet = true;
        assert!(set(&mut settings, "minimal", &Value::Bool(true)));
        assert!(!settings.pet);
    }
}
