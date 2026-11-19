//! 专注会话同步（P4）：deadline 语义 + 单一 running 会话 + 跨机接管。
//!
//! - 本地 `Pomo` 在运行时保存 `deadline`，倒计时按真实时间算，睡眠后不漂移；
//! - 每次开始 / 暂停 / 完成 / 取消都投影成一条 `focus_session` 记录；
//! - **只有本机会话才响铃**：远程会话 adopt 后 `remote=true`，结束只更新显示；
//! - 服务端在 `ops` 里强制「同一账号同时只有一条 running」，新会话把旧的改成
//!   `superseded`；客户端拉到时停止本地计时。
//!
//! 远程会话不回声：`pomo.remote` 期间 `pump` 不生成 op；用户按开始即接管，
//! 换一个新的 session id，服务端自然把旧会话 supersede。

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::app::{App, Phase};
use crate::sync::{self, Ack, Op, Record};

fn meta_path() -> PathBuf {
    crate::model::config_dir().join("focus-sync.json")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct FocusMeta {
    #[serde(default)]
    id: String,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    pending: String,
}

impl FocusMeta {
    fn load() -> Self {
        std::fs::read(meta_path())
            .ok()
            .and_then(|raw| serde_json::from_slice::<FocusMeta>(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(raw) = serde_json::to_vec(self) {
            let _ = crate::model::atomic_write(&meta_path(), &raw);
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FocusPayload {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub end_at: i64,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub remaining: u32,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub completed: bool,
}

fn payload_for(app: &App, state_override: Option<&str>) -> FocusPayload {
    let pomo = &app.pomo;
    let now = Local::now().timestamp();
    let state = state_override.unwrap_or(pomo.state.as_str()).to_string();
    let remaining = if pomo.running {
        pomo.deadline
            .map(|deadline| (deadline - now).max(0) as u32)
            .unwrap_or_else(|| pomo.remaining.max(0.0) as u32)
    } else {
        pomo.remaining.max(0.0) as u32
    };
    FocusPayload {
        phase: match pomo.phase {
            Phase::Focus => "focus",
            Phase::Rest => "rest",
        }
        .into(),
        state,
        started_at: pomo.started_at,
        end_at: if pomo.running {
            pomo.deadline.unwrap_or(now)
        } else {
            pomo.started_at + pomo.duration as i64
        },
        duration: pomo.duration,
        remaining,
        device: pomo.device.clone(),
        completed: matches!(pomo.state.as_str(), "done"),
    }
}

fn hash_payload(payload: &FocusPayload) -> String {
    let mut stable = payload.clone();
    if stable.state == "running" {
        // 运行时 remaining 每秒都变；end_at 才是稳定事实。
        stable.remaining = 0;
    }
    let raw = serde_json::to_vec(&stable).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn apply_acks(meta: &mut FocusMeta, acks: &[Ack]) {
    for ack in acks {
        if meta.pending != ack.op_id {
            continue;
        }
        match ack.status.as_str() {
            "applied" | "duplicate" => {
                meta.rev = ack.rev.unwrap_or(meta.rev.max(1));
                meta.pending.clear();
            }
            // 冲突：服务端记录会回灌 inbox，rev/hash 在 apply_records 里更新。
            _ => meta.pending.clear(),
        }
    }
}

/// 立即取消当前会话并清空本地 id（重置 / 切阶段 / 关闭功能页时调用）。
pub fn cancel_now(app: &mut App) {
    if app.pomo.session_id.is_empty() {
        return;
    }
    if matches!(
        app.pomo.state.as_str(),
        "done" | "cancelled" | "superseded" | "idle"
    ) {
        return;
    }
    let mut meta = FocusMeta::load();
    let payload = payload_for(app, Some("cancelled"));
    let base = if meta.id == app.pomo.session_id {
        meta.rev
    } else {
        0
    };
    let op = Op::new(
        "focus_session",
        &app.pomo.session_id,
        base,
        "",
        serde_json::to_value(&payload).unwrap_or_default(),
    );
    if sync::enqueue(&op).is_ok() {
        meta.id = app.pomo.session_id.clone();
        meta.pending = op.op_id.clone();
        meta.rev = base;
        meta.hash = hash_payload(&payload);
        meta.save();
    }
    app.pomo.running = false;
    app.pomo.deadline = None;
    app.pomo.state = "cancelled".into();
    app.pomo.last_done = None;
    // 清空 id：后续 pump 不再为这条已取消的会话生成 op。
    app.pomo.session_id.clear();
}

/// 每 500ms 泵一次：应用回执，把本地会话状态投影成 op。
pub fn pump(app: &mut App, acks: &[Ack], ready: bool) {
    let mut meta = FocusMeta::load();
    let before = meta.clone();
    apply_acks(&mut meta, acks);

    // 远程会话只显示、不回传；用户按开始即换成新的本地 session id。
    if ready && !app.pomo.remote && !app.pomo.session_id.is_empty() {
        if meta.id != app.pomo.session_id {
            meta = FocusMeta {
                id: app.pomo.session_id.clone(),
                ..Default::default()
            };
        }
        if meta.pending.is_empty() {
            // 完成时 phase 已切到下一阶段，必须优先用结束快照。
            let has_done = app.pomo.last_done.is_some();
            let terminal = has_done
                || matches!(app.pomo.state.as_str(), "running" | "paused");
            if terminal {
                let payload = app
                    .pomo
                    .last_done
                    .clone()
                    .unwrap_or_else(|| payload_for(app, None));
                let hash = hash_payload(&payload);
                if meta.rev == 0 || hash != meta.hash {
                    let op = Op::new(
                        "focus_session",
                        &app.pomo.session_id,
                        meta.rev,
                        "",
                        serde_json::to_value(&payload).unwrap_or_default(),
                    );
                    if sync::enqueue(&op).is_ok() {
                        meta.pending = op.op_id.clone();
                        meta.hash = hash;
                        // op 已持久化到 outbox，结束快照可以放下，避免下一拍重复发送。
                        app.pomo.last_done = None;
                    }
                }
            }
        }
    }

    if meta != before {
        meta.save();
    }
}

fn stop_local(app: &mut App, state: &str) {
    app.pomo.running = false;
    app.pomo.deadline = None;
    app.pomo.state = state.into();
}

/// 应用远端会话记录；返回无法解析、需要留在 inbox 的记录。
pub fn apply_records(app: &mut App, records: &[Record]) -> Vec<Record> {
    let mut meta = FocusMeta::load();
    let before = meta.clone();
    let mut failed = Vec::new();
    let now = Local::now().timestamp();

    for record in records {
        if record.kind != "focus_session" {
            failed.push(record.clone());
            continue;
        }
        let payload: FocusPayload = match serde_json::from_value(record.data.clone()) {
            Ok(payload) => payload,
            Err(_) => {
                failed.push(record.clone());
                continue;
            }
        };
        let hash = hash_payload(&payload);
        let is_local = record.id == app.pomo.session_id;

        if is_local {
            meta.id = record.id.clone();
            meta.rev = record.rev;
            meta.hash = hash;
            meta.pending.clear();
            match payload.state.as_str() {
                "running" => {
                    app.pomo.phase = if payload.phase == "rest" {
                        Phase::Rest
                    } else {
                        Phase::Focus
                    };
                    app.pomo.remaining = payload.remaining as f32;
                    if !app.pomo.running {
                        app.pomo.running = true;
                        app.pomo.deadline = Some(payload.end_at.max(now));
                    }
                    app.pomo.state = "running".into();
                }
                "paused" => {
                    app.pomo.phase = if payload.phase == "rest" {
                        Phase::Rest
                    } else {
                        Phase::Focus
                    };
                    app.pomo.remaining = payload.remaining as f32;
                    app.pomo.last_done = None;
                    stop_local(app, "paused");
                }
                "done" => {
                    if app.pomo.running {
                        // 自己的完成回执：历史已由本地落过，按 id 去重。
                        app.pomo.remote = payload.device != app.sync_device.id;
                        app.finish_focus_phase();
                    } else {
                        app.pomo.state = "done".into();
                    }
                    // 服务端已经有这条 done，不再回声；清掉本地 id，让 pump 停下。
                    app.pomo.last_done = None;
                    app.pomo.session_id.clear();
                }
                "superseded" | "cancelled" => {
                    stop_local(app, &payload.state);
                    app.pomo.last_done = None;
                    app.pomo.session_id.clear();
                }
                _ => {}
            }
        } else if matches!(payload.state.as_str(), "running" | "paused") {
            // 另一台开始 / 恢复：本机停止原会话（不响铃、不记历史），接管显示。
            if !app.pomo.session_id.is_empty() && app.pomo.running {
                app.pomo.state = "superseded".into();
                app.pomo.deadline = None;
            }
            app.pomo.session_id = record.id.clone();
            app.pomo.started_at = payload.started_at;
            app.pomo.duration = payload.duration;
            app.pomo.device = payload.device.clone();
            app.pomo.remote = true;
            app.pomo.last_done = None;
            app.pomo.phase = if payload.phase == "rest" {
                Phase::Rest
            } else {
                Phase::Focus
            };
            app.pomo.remaining = payload.remaining as f32;
            if payload.state == "running" {
                app.pomo.running = true;
                app.pomo.deadline = Some(payload.end_at.max(now));
                app.pomo.state = "running".into();
            } else {
                app.pomo.running = false;
                app.pomo.deadline = None;
                app.pomo.state = "paused".into();
            }
            meta.id = record.id.clone();
            meta.rev = record.rev;
            meta.hash = hash;
            meta.pending.clear();
        } else if payload.state == "done" {
            if payload.phase == "focus" && !app.focus_history.has(&record.id) {
                app.focus_history.append(crate::model::FocusEntry {
                    id: record.id.clone(),
                    phase: payload.phase.clone(),
                    started_at: payload.started_at,
                    ended_at: payload.end_at.max(now),
                    duration: payload.duration,
                    completed: true,
                    device: payload.device.clone(),
                    remote: payload.device != app.sync_device.id,
                });
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

    fn payload(state: &str) -> FocusPayload {
        FocusPayload {
            phase: "focus".into(),
            state: state.into(),
            started_at: 100,
            end_at: 1600,
            duration: 1500,
            remaining: 900,
            device: "dev".into(),
            completed: false,
        }
    }

    #[test]
    fn running_hash_ignores_the_changing_remainder() {
        let mut first = payload("running");
        let mut second = first.clone();
        second.remaining = 300;
        assert_eq!(hash_payload(&first), hash_payload(&second));
        first.remaining = 0;
        assert_eq!(hash_payload(&first), hash_payload(&second));
    }

    #[test]
    fn paused_hash_includes_the_remainder() {
        let first = payload("paused");
        let mut second = first.clone();
        second.remaining = 800;
        assert_ne!(hash_payload(&first), hash_payload(&second));
    }
}
