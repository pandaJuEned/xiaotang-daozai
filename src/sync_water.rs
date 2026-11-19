//! 饮水同步（P3）：一口一条 sip，本地 `water.txt` 只是投影。
//!
//! 为什么不用「同步总量」：两台同时各喝 250 ml，合并需要**加法**；文件级同步只能二选一。
//! 所以服务端存 `water_sip` 事件，客户端把当天未删除的 sip 求和写回 `Water`。
//!
//! 三个容易算错的点：
//! 1. **撤销**：本地 `drunk_ml` 变小 → 删除最新的 sip；整条删多了就补一条差额，
//!    保证投影回本地后总量仍等于用户看到的值。
//! 2. **远端加量 vs 本地撤销**：用 `local_total`（上次投影的总量）区分。
//!    `远端增量 = 当前 sip 总和 - local_total`，`目标 = 本地当前总量 + 远端增量`；
//!    本地自己喝/撤销只体现在「本地当前总量 - local_total」上。
//! 3. **首次接入**：先拉再传。服务端已有 sip 就采纳（第二台不重复计数）；
//!    服务端为空才把本地 `hours[24]` 迁移成 sip（保留柱状图）。

use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::app::App;
use crate::model::Water;
use crate::sync::{self, Ack, Op, Record};

fn meta_path() -> PathBuf {
    crate::model::config_dir().join("water-sync.json")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Sip {
    id: String,
    #[serde(default)]
    day: String,
    #[serde(default)]
    ml: u32,
    #[serde(default)]
    hour: u32,
    #[serde(default)]
    at: i64,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    pending: String,
    /// 在途操作的类型：`create` / `delete`。回执到了才知道该保留还是移除。
    #[serde(default)]
    op_kind: String,
    #[serde(default)]
    deleted: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct WaterMeta {
    /// 当天是否已经做过「首次迁移 / 采纳服务端」；跨天重新判定。
    #[serde(default)]
    migrated_day: String,
    /// 上一次投影到本地的总量。远端增量靠它和当前 sip 总和相减得到。
    #[serde(default)]
    local_total: u32,
    #[serde(default)]
    sips: Vec<Sip>,
}

impl WaterMeta {
    fn load() -> Self {
        std::fs::read(meta_path())
            .ok()
            .and_then(|raw| serde_json::from_slice::<WaterMeta>(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(raw) = serde_json::to_vec(self) {
            let _ = crate::model::atomic_write(&meta_path(), &raw);
        }
    }

    fn fail_pending(&mut self, op_id: &str) {
        for sip in &mut self.sips {
            if sip.pending == op_id {
                sip.pending.clear();
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct WaterPayload {
    #[serde(default)]
    ml: u32,
    #[serde(default)]
    hour: u32,
    #[serde(default)]
    at: i64,
    #[serde(default)]
    day: String,
}

fn sum_today(meta: &WaterMeta, today: &str) -> u32 {
    meta.sips
        .iter()
        .filter(|sip| !sip.deleted && sip.day == today)
        .fold(0u32, |total, sip| total.saturating_add(sip.ml))
}

fn project(water: &mut Water, meta: &WaterMeta, today: &str) {
    let mut hours = [0u32; 24];
    let mut total = 0u32;
    for sip in meta
        .sips
        .iter()
        .filter(|sip| !sip.deleted && sip.day == today)
    {
        let hour = sip.hour.min(23) as usize;
        hours[hour] = hours[hour].saturating_add(sip.ml);
        total = total.saturating_add(sip.ml);
    }
    if water.drunk_ml != total || water.hours != hours || water.start_day != today {
        water.drunk_ml = total;
        water.hours = hours;
        water.start_day = today.to_string();
        water.save();
    }
}

fn epoch_at(day: &str, hour: u32) -> i64 {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(hour.min(23), 0, 0))
        .and_then(|naive| naive.and_local_timezone(chrono::Local).single())
        .map(|at| at.timestamp())
        .unwrap_or_else(|| chrono::Local::now().timestamp())
}

/// 把本地今日总量拆成 sip。优先沿用 `hours[24]`，保留柱状图；没有小时数据时记在当前小时。
fn migrate_local(meta: &mut WaterMeta, water: &Water, today: &str, current_hour: u32) {
    let mut remaining = water.drunk_ml;
    for (hour, &ml) in water.hours.iter().enumerate() {
        if ml == 0 || remaining == 0 {
            continue;
        }
        let take = ml.min(remaining);
        meta.sips.push(Sip {
            id: sync::random_id(),
            day: today.into(),
            ml: take,
            hour: hour as u32,
            at: epoch_at(today, hour as u32),
            ..Default::default()
        });
        remaining -= take;
    }
    if remaining > 0 {
        meta.sips.push(Sip {
            id: sync::random_id(),
            day: today.into(),
            ml: remaining,
            hour: current_hour.min(23),
            at: chrono::Local::now().timestamp(),
            ..Default::default()
        });
    }
}

/// 核心对账：远端增量 + 本地增量 → 目标总量；必要时补/删 sip。
/// 返回 `(ops, blocked)`；`blocked=true` 表示有在途 sip 删不掉，本拍不投影、下拍再试。
fn reconcile(
    water: &mut Water,
    meta: &mut WaterMeta,
    today: &str,
    now: i64,
    current_hour: u32,
) -> (Vec<Op>, bool) {
    let current = water.drunk_ml;
    let sum_before = sum_today(meta, today);

    // 跨天或首次接入：服务端已有数据就采纳，没有才迁移本地。
    if meta.migrated_day != today {
        meta.migrated_day = today.into();
        if sum_before > 0 {
            project(water, meta, today);
            meta.local_total = sum_before;
            return (Vec::new(), false);
        }
        if current > 0 {
            migrate_local(meta, water, today, current_hour);
        }
        meta.local_total = sum_today(meta, today);
        project(water, meta, today);
    }

    let sum_before = sum_today(meta, today);
    let remote_delta = sum_before as i64 - meta.local_total as i64;
    let target = (current as i64 + remote_delta).max(0) as u32;
    let mut sum = sum_before as i64;

    if target > sum as u32 {
        let ml = target - sum as u32;
        meta.sips.push(Sip {
            id: sync::random_id(),
            day: today.into(),
            ml,
            hour: current_hour.min(23),
            at: now,
            ..Default::default()
        });
        sum += ml as i64;
    } else if target < sum as u32 {
        let mut need = sum as u32 - target;
        // 最新的先删；有在途操作的先跳过，等回执后再删。
        let mut order: Vec<usize> = (0..meta.sips.len()).collect();
        order.sort_by_key(|&index| std::cmp::Reverse(meta.sips[index].at));
        for index in order {
            if need == 0 {
                break;
            }
            let (ml, sip_ml, sip_hour, sip_at) = {
                let sip = &mut meta.sips[index];
                if sip.deleted || sip.day != today || !sip.pending.is_empty() {
                    continue;
                }
                let ml = sip.ml.min(need);
                sip.deleted = true;
                (ml, sip.ml, sip.hour, sip.at)
            };
            need -= ml;
            sum -= ml as i64;
            if ml < sip_ml {
                // 整条删多了：补一条差额，保证总量仍等于用户看到的值。
                let leftover = sip_ml - ml;
                meta.sips.push(Sip {
                    id: sync::random_id(),
                    day: today.into(),
                    ml: leftover,
                    hour: sip_hour,
                    at: sip_at,
                    ..Default::default()
                });
                sum += leftover as i64;
                need = 0;
            }
        }
        if need > 0 {
            // 还有量没删掉，但候选都被在途操作占住：本拍不投影，下一拍再试。
            return (Vec::new(), true);
        }
    }

    project(water, meta, today);
    meta.local_total = sum_today(meta, today);
    (Vec::new(), false)
}

fn plan_ops(meta: &mut WaterMeta) -> Vec<Op> {
    let mut ops = Vec::new();
    for sip in &mut meta.sips {
        if !sip.pending.is_empty() {
            continue;
        }
        if sip.deleted {
            if sip.rev == 0 {
                continue; // 从未上传过，本地删掉即可
            }
            let mut op = Op::new("water_sip", &sip.id, sip.rev, &sip.day, serde_json::json!({}));
            op.deleted = true;
            sip.pending = op.op_id.clone();
            sip.op_kind = "delete".into();
            ops.push(op);
        } else if sip.rev == 0 {
            let op = Op::new(
                "water_sip",
                &sip.id,
                0,
                &sip.day,
                serde_json::json!({
                    "ml": sip.ml,
                    "hour": sip.hour,
                    "at": sip.at,
                    "day": sip.day,
                }),
            );
            sip.pending = op.op_id.clone();
            sip.op_kind = "create".into();
            ops.push(op);
        }
    }
    meta.sips
        .retain(|sip| !(sip.deleted && sip.rev == 0 && sip.pending.is_empty()));
    ops
}

fn apply_acks(meta: &mut WaterMeta, acks: &[Ack]) {
    for ack in acks {
        let mut remove = Vec::new();
        for (index, sip) in meta.sips.iter_mut().enumerate() {
            if sip.pending != ack.op_id {
                continue;
            }
            match ack.status.as_str() {
                "applied" | "duplicate" => {
                    if sip.op_kind == "delete" {
                        remove.push(index);
                    } else {
                        // 新建/更新落库：即使本地已经把它标成删除，也要先拿到 rev，
                        // 下一拍再补发删除；直接移除会让服务端留下幽灵 sip。
                        sip.rev = ack.rev.unwrap_or(sip.rev.max(1));
                        sip.pending.clear();
                        sip.op_kind.clear();
                    }
                }
                // 冲突：服务端记录会回灌 inbox，这里只解锁。
                "conflict" => {
                    sip.pending.clear();
                    sip.op_kind.clear();
                }
                _ => {
                    sip.pending.clear();
                    sip.op_kind.clear();
                }
            }
        }
        for index in remove.into_iter().rev() {
            meta.sips.remove(index);
        }
    }
}

fn apply_records(meta: &mut WaterMeta, records: &[Record]) -> Vec<Record> {
    let mut failed = Vec::new();
    for record in records {
        if record.kind != "water_sip" {
            failed.push(record.clone());
            continue;
        }
        if record.deleted {
            meta.sips.retain(|sip| sip.id != record.id);
            continue;
        }
        let payload: WaterPayload = match serde_json::from_value(record.data.clone()) {
            Ok(payload) => payload,
            Err(_) => {
                failed.push(record.clone());
                continue;
            }
        };
        if payload.ml == 0 {
            failed.push(record.clone());
            continue;
        }
        let day = if record.day.is_empty() {
            payload.day.clone()
        } else {
            record.day.clone()
        };
        if let Some(sip) = meta.sips.iter_mut().find(|sip| sip.id == record.id) {
            sip.day = day;
            sip.ml = payload.ml;
            sip.hour = payload.hour;
            sip.at = payload.at;
            sip.rev = record.rev;
            sip.deleted = false;
            sip.pending.clear();
            sip.op_kind.clear();
        } else {
            meta.sips.push(Sip {
                id: record.id.clone(),
                day,
                ml: payload.ml,
                hour: payload.hour,
                at: payload.at,
                rev: record.rev,
                pending: String::new(),
                op_kind: String::new(),
                deleted: false,
            });
        }
    }
    failed
}

fn prune(meta: &mut WaterMeta, today: &str) {
    let cutoff = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d")
        .ok()
        .map(|date| (date - chrono::Duration::days(1)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| today.to_string());
    meta.sips
        .retain(|sip| !sip.pending.is_empty() || sip.day >= cutoff);
}

/// `sync_map` 每 500ms 调一次：先应用远端 water 记录，再做本地对账。
/// `acks` 由调用方统一排水后传入；这里只认自己 sip 的 op_id。
/// 返回无法解析、需要留在 inbox 的记录。
pub fn sync(app: &mut App, records: &[Record], ready: bool, acks: &[Ack]) -> Vec<Record> {
    let mut meta = WaterMeta::load();
    let before = meta.clone();
    apply_acks(&mut meta, acks);
    let failed = apply_records(&mut meta, records);

    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let current_hour = now.hour();
    if ready {
        prune(&mut meta, &today);
        let (mut ops, blocked) = reconcile(
            &mut app.water,
            &mut meta,
            &today,
            now.timestamp(),
            current_hour,
        );
        if !blocked {
            ops.extend(plan_ops(&mut meta));
        }
        for op in ops {
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

    fn water(total: u32, hours: [u32; 24]) -> Water {
        Water {
            drunk_ml: total,
            hours,
            start_day: "2026-11-19".into(),
            ..Default::default()
        }
    }

    #[test]
    fn migration_keeps_the_hourly_histogram() {
        let mut hours = [0u32; 24];
        hours[9] = 250;
        hours[14] = 250;
        let mut water = water(500, hours);
        let mut meta = WaterMeta::default();
        reconcile(&mut water, &mut meta, "2026-11-19", 1000, 15);
        assert_eq!(water.drunk_ml, 500);
        assert_eq!(meta.sips.len(), 2);
        assert_eq!(sum_today(&meta, "2026-11-19"), 500);
        assert_eq!(meta.local_total, 500);
        let ops = plan_ops(&mut meta);
        assert_eq!(ops.len(), 2, "迁移出的两条 sip 都应上传");
        assert!(ops.iter().all(|op| op.kind == "water_sip" && !op.deleted));
    }

    #[test]
    fn remote_sip_is_projected_without_a_local_delta() {
        let mut meta = WaterMeta {
            migrated_day: "2026-11-19".into(),
            local_total: 500,
            sips: vec![Sip {
                id: "a".into(),
                day: "2026-11-19".into(),
                ml: 500,
                hour: 9,
                at: 100,
                rev: 1,
                ..Default::default()
            }],
        };
        let mut water = water(500, [0; 24]);
        // 另一台又喝了 250，远端记录已进 meta。
        meta.sips.push(Sip {
            id: "b".into(),
            day: "2026-11-19".into(),
            ml: 250,
            hour: 10,
            at: 200,
            rev: 1,
            ..Default::default()
        });
        reconcile(&mut water, &mut meta, "2026-11-19", 300, 10);
        assert_eq!(water.drunk_ml, 750, "远端加量应投影到本地，且不能被当成撤销");
        assert_eq!(water.hours[10], 250);
        assert_eq!(meta.local_total, 750);
    }

    #[test]
    fn local_drink_creates_one_sip() {
        let mut meta = WaterMeta {
            migrated_day: "2026-11-19".into(),
            local_total: 500,
            sips: vec![Sip {
                id: "a".into(),
                day: "2026-11-19".into(),
                ml: 500,
                hour: 9,
                at: 100,
                rev: 1,
                ..Default::default()
            }],
        };
        let mut water = water(750, [0; 24]);
        reconcile(&mut water, &mut meta, "2026-11-19", 300, 10);
        assert_eq!(sum_today(&meta, "2026-11-19"), 750);
        let ops = plan_ops(&mut meta);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].data["ml"], 250);
    }

    #[test]
    fn undo_deletes_the_newest_and_rebalances_overshoot() {
        let mut meta = WaterMeta {
            migrated_day: "2026-11-19".into(),
            local_total: 500,
            sips: vec![Sip {
                id: "a".into(),
                day: "2026-11-19".into(),
                ml: 500,
                hour: 9,
                at: 100,
                rev: 1,
                ..Default::default()
            }],
        };
        let mut water = water(250, [0; 24]);
        reconcile(&mut water, &mut meta, "2026-11-19", 300, 10);
        assert_eq!(water.drunk_ml, 250);
        assert_eq!(sum_today(&meta, "2026-11-19"), 250);
        let ops = plan_ops(&mut meta);
        assert!(ops.iter().any(|op| op.deleted && op.id == "a"), "旧 sip 应删除");
        assert!(ops.iter().any(|op| !op.deleted), "差额应补一条 250");
    }

    #[test]
    fn first_sync_adopts_server_and_does_not_double_count() {
        // 第二台本地 300，服务端已有 500：采纳 500，而不是 300+500。
        let mut meta = WaterMeta {
            sips: vec![Sip {
                id: "server".into(),
                day: "2026-11-19".into(),
                ml: 500,
                hour: 9,
                at: 100,
                rev: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut water = water(300, [0; 24]);
        reconcile(&mut water, &mut meta, "2026-11-19", 300, 10);
        assert_eq!(water.drunk_ml, 500);
        assert_eq!(meta.local_total, 500);
        assert_eq!(meta.sips.len(), 1, "不能把本地 300 再迁一条");
    }

    #[test]
    fn pending_sip_blocks_projection_until_ack() {
        let mut meta = WaterMeta {
            migrated_day: "2026-11-19".into(),
            local_total: 500,
            sips: vec![Sip {
                id: "a".into(),
                day: "2026-11-19".into(),
                ml: 500,
                hour: 9,
                at: 100,
                rev: 1,
                pending: "op-1".into(),
                ..Default::default()
            }],
        };
        let mut water = water(250, [0; 24]);
        let (_, blocked) = reconcile(&mut water, &mut meta, "2026-11-19", 300, 10);
        assert!(blocked, "在途 sip 删不掉时应阻塞，不能把本地 250 覆盖成 500");
        assert_eq!(water.drunk_ml, 250);
    }

    #[test]
    fn undo_before_create_ack_still_sends_delete() {
        let mut meta = WaterMeta {
            migrated_day: "2026-11-19".into(),
            local_total: 250,
            sips: vec![Sip {
                id: "a".into(),
                day: "2026-11-19".into(),
                ml: 250,
                hour: 9,
                at: 100,
                rev: 0,
                pending: "op-create".into(),
                op_kind: "create".into(),
                deleted: true,
            }],
        };
        apply_acks(
            &mut meta,
            &[Ack {
                op_id: "op-create".into(),
                status: "applied".into(),
                rev: Some(1),
                seq: Some(1),
            }],
        );
        assert_eq!(meta.sips.len(), 1, "新建回执不能把已撤销的 sip 直接丢掉");
        assert_eq!(meta.sips[0].rev, 1);
        let ops = plan_ops(&mut meta);
        assert_eq!(ops.len(), 1);
        assert!(ops[0].deleted, "拿到 rev 后要补发删除");
    }

    #[test]
    fn remote_delete_reduces_the_projected_total() {
        let mut meta = WaterMeta {
            migrated_day: "2026-11-19".into(),
            local_total: 750,
            sips: vec![
                Sip { id: "a".into(), day: "2026-11-19".into(), ml: 500, hour: 9, at: 100, rev: 1, ..Default::default() },
                Sip { id: "b".into(), day: "2026-11-19".into(), ml: 250, hour: 10, at: 200, rev: 1, ..Default::default() },
            ],
        };
        let mut water = water(750, [0; 24]);
        // 另一台撤销了 250：服务端记录已删除，meta 里也删掉了 b。
        meta.sips.retain(|sip| sip.id != "b");
        reconcile(&mut water, &mut meta, "2026-11-19", 300, 11);
        assert_eq!(water.drunk_ml, 500);
        assert_eq!(meta.local_total, 500);
    }
}
