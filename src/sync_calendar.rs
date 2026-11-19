//! 本机日程同步（P5）：`local_event` 记录，按 `uid` 对齐。
//!
//! `local-calendar.json` 格式不变；侧车 `calendar-sync.json` 存每条事件的
//! `hash / rev / pending`。事件本身有稳定 `uid`，不需要 LCS：
//! 本地有、侧车没有 → 新建；内容哈希变了 → 更新；侧车有、本地没有 → 删除。
//!
//! 首轮先应用服务端再上传，两台各自独有的日程会合并；同 uid 以服务端为准。

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::app::App;
use crate::local_calendar::{self, Entry};
use crate::mail::Meeting;
use crate::sync::{self, Ack, Op, Record};

fn meta_path() -> PathBuf {
    crate::model::config_dir().join("calendar-sync.json")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct EntryMeta {
    uid: String,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    pending: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Tomb {
    uid: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    pending: String,
    /// `update` / `delete`：更新回执到了要保留，删除回执到了才丢弃。
    #[serde(default)]
    kind: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Meta {
    #[serde(default)]
    entries: Vec<EntryMeta>,
    #[serde(default)]
    tombstones: Vec<Tomb>,
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

    fn fail_pending(&mut self, op_id: &str) {
        for entry in &mut self.entries {
            if entry.pending == op_id {
                entry.pending.clear();
                entry.hash.clear();
            }
        }
        for tomb in &mut self.tombstones {
            if tomb.pending == op_id {
                tomb.pending.clear();
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct EventPayload {
    #[serde(default)]
    title: String,
    #[serde(default)]
    start: i64,
    #[serde(default)]
    end: Option<i64>,
    #[serde(default)]
    all_day: bool,
    #[serde(default)]
    reminder: Option<u32>,
}

impl EventPayload {
    fn from_entry(entry: &Entry) -> Self {
        Self {
            title: entry.event.title.clone(),
            start: entry.event.start,
            end: entry.event.end,
            all_day: entry.event.all_day,
            reminder: entry.reminder,
        }
    }

    fn into_entry(&self, uid: &str) -> Entry {
        Entry {
            event: Meeting {
                uid: uid.into(),
                title: self.title.clone(),
                start: self.start,
                end: self.end,
                all_day: self.all_day,
                at: chrono::DateTime::from_timestamp(self.start, 0)
                    .map(|at| at.with_timezone(&Local).format("%m-%d %H:%M").to_string())
                    .unwrap_or_default(),
            },
            reminder: self.reminder,
        }
    }

    fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

fn hash_payload(payload: &EventPayload) -> String {
    let raw = serde_json::to_vec(payload).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn plan_ops(local: &[Entry], meta: &mut Meta) -> Vec<Op> {
    let mut ops = Vec::new();

    // 先处理删除：如果用户删掉后又加回同一个 uid，删除 op 会排在新建之前，
    // 服务端先删后建，最终结果才是「加回来了」。
    let known: std::collections::HashSet<&str> = local
        .iter()
        .map(|entry| entry.event.uid.as_str())
        .collect();
    let mut gone = Vec::new();
    meta.entries.retain(|item| {
        if known.contains(item.uid.as_str()) {
            true
        } else {
            gone.push((item.uid.clone(), item.rev, item.pending.clone()));
            false
        }
    });
    for (uid, rev, pending) in gone {
        if rev == 0 && pending.is_empty() {
            continue; // 从未上传过
        }
        meta.tombstones.push(Tomb {
            uid,
            rev,
            kind: if pending.is_empty() { "delete".into() } else { "update".into() },
            pending,
        });
    }

    // 没有在途操作的墓碑才发删除；在途更新等回执后再删。
    let mut keep = Vec::new();
    for mut tomb in std::mem::take(&mut meta.tombstones) {
        if !tomb.pending.is_empty() {
            keep.push(tomb);
            continue;
        }
        if tomb.rev == 0 {
            continue;
        }
        let mut op = Op::new("local_event", &tomb.uid, tomb.rev, "", serde_json::json!({}));
        op.deleted = true;
        tomb.pending = op.op_id.clone();
        tomb.kind = "delete".into();
        keep.push(tomb);
        ops.push(op);
    }
    meta.tombstones = keep;

    for entry in local {
        let uid = entry.event.uid.as_str();
        if uid.is_empty() {
            continue;
        }
        let payload = EventPayload::from_entry(entry);
        let hash = hash_payload(&payload);
        if let Some(existing) = meta.entries.iter_mut().find(|item| item.uid == uid) {
            if !existing.pending.is_empty() || (existing.rev > 0 && hash == existing.hash) {
                continue;
            }
            let op = Op::new("local_event", uid, existing.rev, "", payload.value());
            existing.pending = op.op_id.clone();
            existing.hash = hash;
            ops.push(op);
        } else {
            let op = Op::new("local_event", uid, 0, "", payload.value());
            meta.entries.push(EntryMeta {
                uid: uid.into(),
                hash,
                rev: 0,
                pending: op.op_id.clone(),
            });
            ops.push(op);
        }
    }
    ops
}

fn apply_acks(meta: &mut Meta, acks: &[Ack]) {
    for ack in acks {
        for entry in &mut meta.entries {
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
        let mut keep = Vec::new();
        for mut tomb in std::mem::take(&mut meta.tombstones) {
            if tomb.pending != ack.op_id {
                keep.push(tomb);
                continue;
            }
            match ack.status.as_str() {
                "applied" | "duplicate" => {
                    if tomb.kind == "delete" {
                        // 删除已落库
                    } else {
                        tomb.rev = ack.rev.unwrap_or(tomb.rev.max(1));
                        tomb.pending.clear();
                        keep.push(tomb);
                    }
                }
                // 冲突：服务端记录会回灌，墓碑不再需要。
                "conflict" => {}
                _ => {
                    tomb.pending.clear();
                    keep.push(tomb);
                }
            }
        }
        meta.tombstones = keep;
    }
}

/// 应用远端事件，返回 (是否改动, 无法解析的记录)。
fn apply_records(app: &mut App, meta: &mut Meta, records: &[Record]) -> (bool, Vec<Record>) {
    let mut changed = false;
    let mut failed = Vec::new();
    for record in records {
        if record.kind != "local_event" {
            failed.push(record.clone());
            continue;
        }
        let uid = record.id.as_str();
        if record.deleted {
            let before = app.calendar.local.len();
            app.calendar.local.retain(|entry| entry.event.uid != uid);
            if app.calendar.local.len() != before {
                changed = true;
            }
            meta.entries.retain(|item| item.uid != uid);
            meta.tombstones.retain(|tomb| tomb.uid != uid);
            app.alerts
                .retain(|alert| !alert.meeting.as_ref().is_some_and(|(event, _)| event.uid == uid));
            continue;
        }
        let payload: EventPayload = match serde_json::from_value(record.data.clone()) {
            Ok(payload) => payload,
            Err(_) => {
                failed.push(record.clone());
                continue;
            }
        };
        if payload.title.trim().is_empty() {
            failed.push(record.clone());
            continue;
        }
        let entry = payload.into_entry(uid);
        let hash = hash_payload(&payload);
        if let Some(existing) = app
            .calendar
            .local
            .iter_mut()
            .find(|item| item.event.uid == uid)
        {
            *existing = entry;
        } else {
            if app.calendar.local.len() >= 5000 {
                failed.push(record.clone());
                continue;
            }
            app.calendar.local.push(entry);
        }
        if let Some(item) = meta.entries.iter_mut().find(|item| item.uid == uid) {
            item.rev = record.rev;
            item.hash = hash;
            item.pending.clear();
        } else {
            meta.entries.push(EntryMeta {
                uid: uid.into(),
                hash,
                rev: record.rev,
                pending: String::new(),
            });
        }
        meta.tombstones.retain(|tomb| tomb.uid != uid);
        app.mail
            .fired
            .remove(&crate::calendar::dedupe_key(&payload.into_entry(uid).event));
        changed = true;
    }
    if changed {
        let _ = local_calendar::save(&app.calendar.local);
    }
    (changed, failed)
}

/// `sync_map` 每 500ms 调一次。返回需要留在 inbox 的记录。
pub fn sync(app: &mut App, records: &[Record], acks: &[Ack], ready: bool) -> Vec<Record> {
    let mut meta = Meta::load();
    let before = meta.clone();
    apply_acks(&mut meta, acks);
    let (_, failed) = apply_records(app, &mut meta, records);

    if ready {
        for op in plan_ops(&app.calendar.local, &mut meta) {
            if sync::enqueue(&op).is_err() {
                meta.fail_pending(&op.op_id);
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

    fn entry(uid: &str, title: &str, start: i64) -> Entry {
        Entry {
            event: Meeting {
                uid: uid.into(),
                title: title.into(),
                start,
                end: Some(start + 3600),
                all_day: false,
                at: String::new(),
            },
            reminder: Some(15),
        }
    }

    #[test]
    fn hash_ignores_derived_display_text() {
        let first = EventPayload::from_entry(&entry("e1", "组会", 100));
        let mut second = first.clone();
        second.title = "组会".into();
        assert_eq!(hash_payload(&first), hash_payload(&second));
    }

    #[test]
    fn plan_creates_updates_and_deletes() {
        let mut meta = Meta::default();
        let mut local = vec![entry("e1", "组会", 100)];
        let ops = plan_ops(&local, &mut meta);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].base_rev, 0);

        // 回执后改标题 → 更新
        apply_acks(
            &mut meta,
            &[Ack {
                op_id: ops[0].op_id.clone(),
                status: "applied".into(),
                rev: Some(1),
                seq: Some(1),
            }],
        );
        local[0].event.title = "组会（改）".into();
        let ops = plan_ops(&local, &mut meta);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].base_rev, 1);

        // 本地删掉 → 墓碑 + 删除 op
        apply_acks(
            &mut meta,
            &[Ack {
                op_id: ops[0].op_id.clone(),
                status: "applied".into(),
                rev: Some(2),
                seq: Some(2),
            }],
        );
        local.clear();
        let ops = plan_ops(&local, &mut meta);
        assert_eq!(ops.len(), 1);
        assert!(ops[0].deleted);
    }
}
