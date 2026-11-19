//! 待办同步（P2）：稳定 id、逐条 rev、应用远端记录、生成 outbox 操作。
//!
//! 为什么用「对账」而不是在每个增删改入口埋点：
//! - 待办有添加、编辑、勾选、删除、提醒、外部手改六条路径，逐个埋点必漏；
//! - 每秒拿当前 `todo.txt` 与上次同步快照对比，差异就是要上传的操作，
//!   离线期间的所有修改也会在恢复后一次补齐。
//!
//! 身份：`todo-ids.json` 与 `todo.txt` 行序对齐，保存 `id / hash / rev / pending`。
//! 应用内编辑按行序保留 id；外部插入/删除靠 hash 匹配，匹配不到的才新建或墓碑。
//!
//! 冲突：服务端胜。服务端把整条现状塞回 `sync-inbox.jsonl`，这里照常应用；
//! 本地版本已由 `sync.rs` 写入 `sync-conflicts-YYYY-MM-DD.txt`。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::app::App;
use crate::model::{TodoItem, TodoList};
use crate::sync::{self, Ack, Op, Record};

fn meta_path() -> PathBuf {
    crate::model::config_dir().join("todo-ids.json")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Entry {
    id: String,
    /// 上一次同步时的内容哈希；用来判断本地是否改过。
    #[serde(default)]
    hash: String,
    #[serde(default)]
    rev: i64,
    /// 在途操作的 `op_id`；非空时不再生成新操作，等回执。
    #[serde(default)]
    pending: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Tomb {
    id: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    pending: String,
    /// `update` / `delete` / `create`：回执到了要区分「更新已落库」与「删除已完成」。
    #[serde(default)]
    kind: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Meta {
    #[serde(default)]
    items: Vec<Entry>,
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

    /// 入队失败：解锁并把 hash 清掉，下一拍重新对账时还能再生成一次操作。
    fn fail_pending(&mut self, op_id: &str) {
        for entry in &mut self.items {
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
struct Payload {
    #[serde(default)]
    text: String,
    #[serde(default)]
    done_on: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    repeat: u8,
    #[serde(default)]
    reminded: bool,
    #[serde(default)]
    reminded_on: Option<String>,
    /// 跨设备的显示顺序：源行序 × 100。
    #[serde(default)]
    sort: i64,
}

impl Payload {
    fn from_item(item: &TodoItem, index: usize) -> Self {
        Self {
            text: item.text.clone(),
            done_on: item.done_on.clone(),
            due: item.due.clone(),
            repeat: item.repeat,
            reminded: item.reminded,
            reminded_on: item.reminded_on.clone(),
            sort: index as i64 * 100,
        }
    }

    fn to_item(&self) -> TodoItem {
        TodoItem {
            text: self.text.clone(),
            done_on: self.done_on.clone(),
            due: self.due.clone(),
            repeat: self.repeat,
            reminded: self.reminded,
            reminded_on: self.reminded_on.clone(),
        }
    }

    fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

fn hash_payload(payload: &Payload) -> String {
    // 行序（sort）不进哈希：删除/插入一行会让后面所有行的下标变化，
    // 若把 sort 算进去，每次移动都会误判成「内容改了」而触发整列表更新。
    let mut identity = payload.clone();
    identity.sort = 0;
    let raw = serde_json::to_vec(&identity).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------- 身份对齐

/// 让 `meta.items` 与 `todos` 逐行对齐。
///
/// 用最长公共子序列（LCS）找「没改过的行」作锚点：
/// - 锚点之间剩下的行按顺序两两配对（应用内改文本，id 不变）；
/// - 多出来的当前行是新条目，多出来的旧条目转墓碑；
/// - 这样「先改一条、再在别处插一条」也不会把改过的那条当成新条目。
fn align(todos: &[TodoItem], meta: &mut Meta) {
    let old = std::mem::take(&mut meta.items);
    let hashes: Vec<String> = todos
        .iter()
        .enumerate()
        .map(|(index, item)| hash_payload(&Payload::from_item(item, index)))
        .collect();
    let old_hashes: Vec<String> = old.iter().map(|entry| entry.hash.clone()).collect();
    let anchors = lcs_pairs(&old_hashes, &hashes);

    let mut matched: Vec<Option<usize>> = vec![None; todos.len()];
    let mut used = vec![false; old.len()];
    for &(old_index, new_index) in &anchors {
        matched[new_index] = Some(old_index);
        used[old_index] = true;
    }

    // 锚点把两边切成若干段，段内按顺序配对：前面的是编辑，多出来的是新增/删除。
    let mut boundaries = Vec::with_capacity(anchors.len() + 1);
    boundaries.push((0usize, 0usize));
    for &(old_index, new_index) in &anchors {
        boundaries.push((old_index + 1, new_index + 1));
    }
    let mut old_start = 0usize;
    let mut new_start = 0usize;
    for (next_old, next_new) in boundaries.into_iter().skip(1).chain(std::iter::once((old.len(), todos.len()))) {
        let old_slice: Vec<usize> = (old_start..next_old).filter(|&index| !used[index]).collect();
        let new_slice: Vec<usize> = (new_start..next_new).filter(|&index| matched[index].is_none()).collect();
        for (&slot, &item_index) in old_slice.iter().zip(new_slice.iter()) {
            matched[item_index] = Some(slot);
            used[slot] = true;
        }
        old_start = next_old;
        new_start = next_new;
    }

    let mut rebuilt = Vec::with_capacity(todos.len());
    for (index, _item) in todos.iter().enumerate() {
        match matched[index] {
            Some(slot) => {
                let entry = &old[slot];
                rebuilt.push(Entry {
                    id: entry.id.clone(),
                    // 保留旧 hash：改过的内容会在 plan_ops 里被识别出来。
                    hash: entry.hash.clone(),
                    rev: entry.rev,
                    pending: entry.pending.clone(),
                });
            }
            None => rebuilt.push(Entry {
                id: sync::random_id(),
                // 新条目直接记下当前内容哈希，下一拍才能作为 LCS 锚点。
                // rev=0 时 plan_ops 仍会走「新建」，不看 hash。
                hash: hashes[index].clone(),
                rev: 0,
                pending: String::new(),
            }),
        }
    }

    for (slot, entry) in old.iter().enumerate() {
        if used[slot] {
            continue;
        }
        if entry.rev == 0 && entry.pending.is_empty() {
            continue;
        }
        meta.tombstones.push(Tomb {
            id: entry.id.clone(),
            rev: entry.rev,
            pending: entry.pending.clone(),
            kind: if entry.pending.is_empty() { "delete".into() } else { "update".into() },
        });
    }
    meta.items = rebuilt;
}

/// 最长公共子序列的匹配下标对；内容相同的行才可能成为锚点。
fn lcs_pairs(old: &[String], new: &[String]) -> Vec<(usize, usize)> {
    let rows = old.len();
    let cols = new.len();
    let mut dp = vec![vec![0usize; cols + 1]; rows + 1];
    for i in (0..rows).rev() {
        for j in (0..cols).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < rows && j < cols {
        if old[i] == new[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

// ---------------------------------------------------------------- 生成操作

fn plan_ops(todos: &[TodoItem], meta: &mut Meta) -> Vec<Op> {
    let mut ops = Vec::new();
    for (index, item) in todos.iter().enumerate() {
        let Some(entry) = meta.items.get_mut(index) else {
            continue;
        };
        if !entry.pending.is_empty() {
            continue;
        }
        let payload = Payload::from_item(item, index);
        let hash = hash_payload(&payload);
        if entry.rev == 0 {
            let op = Op::new("todo_item", &entry.id, 0, "", payload.value());
            entry.pending = op.op_id.clone();
            entry.hash = hash;
            ops.push(op);
        } else if hash != entry.hash {
            let op = Op::new("todo_item", &entry.id, entry.rev, "", payload.value());
            entry.pending = op.op_id.clone();
            entry.hash = hash;
            ops.push(op);
        }
    }

    let mut keep = Vec::new();
    for mut tomb in std::mem::take(&mut meta.tombstones) {
        if !tomb.pending.is_empty() {
            keep.push(tomb);
            continue;
        }
        if tomb.rev == 0 {
            continue;
        }
        let mut op = Op::new("todo_item", &tomb.id, tomb.rev, "", serde_json::json!({}));
        op.deleted = true;
        tomb.pending = op.op_id.clone();
        tomb.kind = "delete".into();
        keep.push(tomb);
        ops.push(op);
    }
    meta.tombstones = keep;
    ops
}

// ---------------------------------------------------------------- 应用

fn apply_acks(meta: &mut Meta, acks: &[Ack]) {
    for ack in acks {
        for entry in &mut meta.items {
            if entry.pending != ack.op_id {
                continue;
            }
            match ack.status.as_str() {
                "applied" | "duplicate" => {
                    entry.rev = ack.rev.unwrap_or(entry.rev.max(1));
                    entry.pending.clear();
                }
                // 冲突：rev 由服务端记录（马上会进 inbox）更新，这里只解锁。
                "conflict" => entry.pending.clear(),
                _ => {
                    // 其它状态（服务端拒绝 / 未知）保留重试机会。
                    entry.pending.clear();
                    entry.hash.clear();
                }
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
                        // 删除已落库，墓碑可以丢了。
                    } else {
                        tomb.rev = ack.rev.unwrap_or(tomb.rev.max(1));
                        tomb.pending.clear();
                        keep.push(tomb);
                    }
                }
                // 服务端胜：记录会回灌 inbox；墓碑不再需要。
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

/// 应用远端待办记录，返回 (是否改动, 无法解析、需要留在 inbox 的记录)。
fn apply_records(todos: &mut TodoList, meta: &mut Meta, records: &[Record]) -> (bool, Vec<Record>) {
    let mut changed = false;
    let mut failed = Vec::new();
    for record in records {
        if record.kind != "todo_item" {
            failed.push(record.clone());
            continue;
        }
        if record.deleted {
            if let Some(index) = meta.items.iter().position(|entry| entry.id == record.id) {
                if index < todos.items.len() {
                    todos.items.remove(index);
                    changed = true;
                }
                meta.items.remove(index);
            }
            meta.tombstones.retain(|tomb| tomb.id != record.id);
            continue;
        }
        let payload: Payload = match serde_json::from_value(record.data.clone()) {
            Ok(payload) => payload,
            Err(_) => {
                failed.push(record.clone());
                continue;
            }
        };
        let item = payload.to_item();
        if item.text.trim().is_empty() {
            failed.push(record.clone());
            continue;
        }
        let hash = hash_payload(&payload);
        if let Some(index) = meta.items.iter().position(|entry| entry.id == record.id) {
            if index < todos.items.len() {
                todos.items[index] = item;
            } else {
                todos.items.push(item);
            }
            let entry = &mut meta.items[index];
            entry.rev = record.rev;
            entry.hash = hash;
            entry.pending.clear();
            changed = true;
        } else {
            let at = (payload.sort.max(0) / 100) as usize;
            let at = at.min(todos.items.len());
            todos.items.insert(at, item);
            meta.items.insert(
                at,
                Entry {
                    id: record.id.clone(),
                    hash,
                    rev: record.rev,
                    pending: String::new(),
                },
            );
            meta.tombstones.retain(|tomb| tomb.id != record.id);
            changed = true;
        }
    }
    (changed, failed)
}

// ---------------------------------------------------------------- 主泵

/// UI 线程每 500ms 调一次：先采纳远端，再把本地差异推入 outbox。
pub fn pump(app: &mut App) {
    if !app.settings.sync_enabled {
        return;
    }
    // 待办正在输入 / 编辑 / 改提醒时不动列表，避免下标错位；提交后下一拍再对账。
    // 饮水不受这个限制，照常同步。
    let todo_busy = app.adding || app.editing.is_some() || app.todo_reminder.is_some();
    let ready = app.sync.ready;

    let mut meta = Meta::load();
    let before = meta.clone();

    let acks = sync::drain_acks();
    apply_acks(&mut meta, &acks);

    let records = sync::read_inbox();
    let had_records = !records.is_empty();
    let (todo_records, rest): (Vec<Record>, Vec<Record>) = records
        .into_iter()
        .partition(|record| record.kind == "todo_item");
    let (water_records, rest): (Vec<Record>, Vec<Record>) = rest
        .into_iter()
        .partition(|record| record.kind == "water_sip");
    let (focus_records, rest): (Vec<Record>, Vec<Record>) = rest
        .into_iter()
        .partition(|record| record.kind == "focus_session");
    let (calendar_records, rest): (Vec<Record>, Vec<Record>) = rest
        .into_iter()
        .partition(|record| record.kind == "local_event");
    let (setting_records, others): (Vec<Record>, Vec<Record>) = rest
        .into_iter()
        .partition(|record| record.kind == "setting");

    // 饮水：应用远端 sip、按本地总量对账并生成操作（内部自己管 meta）。
    let water_failed = crate::sync_water::sync(app, &water_records, ready, &acks);
    // 专注：先采纳远端会话，再把本地会话状态投影成 op。
    let focus_failed = crate::sync_focus::apply_records(app, &focus_records);
    crate::sync_focus::pump(app, &acks, ready);
    // 本机日程与账号级设置。
    let calendar_failed = crate::sync_calendar::sync(app, &calendar_records, &acks, ready);
    let settings_failed = crate::sync_settings::sync(app, &setting_records, &acks, ready);

    let mut changed = false;
    let mut remain = others;
    if !todo_records.is_empty() {
        if todo_busy {
            // 编辑期间先留在 inbox，提交后下一拍再应用。
            remain.extend(todo_records);
        } else {
            align(&app.todos.items, &mut meta);
            let (applied, failed) = apply_records(&mut app.todos, &mut meta, &todo_records);
            changed = applied;
            remain.extend(failed);
        }
    }
    remain.extend(water_failed);
    remain.extend(focus_failed);
    remain.extend(calendar_failed);
    remain.extend(settings_failed);
    if had_records {
        sync::replace_inbox(&remain);
    }

    align(&app.todos.items, &mut meta);
    // 首次拉取完成前不上传：第二台设备要先看到服务端已有条目，避免重复创建。
    if ready && !todo_busy {
        for op in plan_ops(&app.todos.items, &mut meta) {
            if sync::enqueue(&op).is_err() {
                meta.fail_pending(&op.op_id);
            }
        }
    }

    if changed {
        app.todos.save();
    }
    if meta != before {
        meta.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> TodoItem {
        TodoItem {
            text: text.into(),
            ..Default::default()
        }
    }

    fn meta_for(todos: &[TodoItem]) -> Meta {
        let mut meta = Meta::default();
        align(todos, &mut meta);
        meta
    }

    #[test]
    fn align_keeps_ids_for_edits_and_assigns_new_for_inserts() {
        let mut todos = vec![item("刷牙"), item("交报告"), item("买牛奶")];
        let mut meta = meta_for(&todos);
        let ids: Vec<String> = meta.items.iter().map(|entry| entry.id.clone()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| !id.is_empty()));

        // 应用内改文本：行数不变，按行序保留 id。
        todos[1].text = "交实验报告".into();
        align(&todos, &mut meta);
        assert_eq!(meta.items[1].id, ids[1], "编辑不应换 id");

        // 顶部插入一条：未改动的三条按 hash 找回原 id，新条目拿新 id。
        todos.insert(0, item("取快递"));
        align(&todos, &mut meta);
        assert_eq!(meta.items.len(), 4);
        assert_ne!(meta.items[0].id, ids[0]);
        assert_eq!(meta.items[1].id, ids[0]);
        assert_eq!(meta.items[2].id, ids[1]);
        assert_eq!(meta.items[3].id, ids[2]);

        // 删除最后一条：其余 id 不变，旧条目进墓碑或直接丢弃。
        todos.pop();
        align(&todos, &mut meta);
        assert_eq!(meta.items.len(), 3);
        assert_eq!(meta.items[1].id, ids[0]);
    }

    #[test]
    fn plan_ops_creates_updates_and_deletes_only_once() {
        let mut todos = vec![item("A"), item("B")];
        let mut meta = Meta::default();
        align(&todos, &mut meta);
        meta.items[0].rev = 3;
        meta.items[0].hash = hash_payload(&Payload::from_item(&todos[0], 0));
        meta.items[1].rev = 0;

        todos[0].text = "A2".into();
        let ops = plan_ops(&todos, &mut meta);
        assert_eq!(ops.len(), 2, "一条更新 + 一条新建");
        let update = ops.iter().find(|op| op.id == meta.items[0].id).unwrap();
        assert_eq!(update.base_rev, 3);
        assert!(!update.deleted);
        assert!(meta.items.iter().all(|entry| !entry.pending.is_empty()));
        assert!(plan_ops(&todos, &mut meta).is_empty(), "在途时不得重复入队");

        // 回执：更新落库、新建拿到 rev=1。
        let acks: Vec<Ack> = ops
            .iter()
            .map(|op| Ack {
                op_id: op.op_id.clone(),
                status: "applied".into(),
                rev: Some(if op.base_rev == 0 { 1 } else { 4 }),
                seq: Some(1),
            })
            .collect();
        apply_acks(&mut meta, &acks);
        assert_eq!(meta.items[0].rev, 4);
        assert_eq!(meta.items[1].rev, 1);
        assert!(meta.items.iter().all(|entry| entry.pending.is_empty()));
        assert!(plan_ops(&todos, &mut meta).is_empty(), "内容没变不应再上传");
    }

    #[test]
    fn delete_goes_out_after_the_inflight_update_is_acked() {
        let mut todos = vec![item("A"), item("B")];
        let mut meta = Meta::default();
        align(&todos, &mut meta);
        meta.items[0].rev = 2;
        meta.items[0].hash = hash_payload(&Payload::from_item(&todos[0], 0));
        meta.items[1].rev = 2;
        meta.items[1].hash = hash_payload(&Payload::from_item(&todos[1], 1));

        todos[0].text = "A2".into();
        let ops = plan_ops(&todos, &mut meta);
        assert_eq!(ops.len(), 1);

        // 更新还没回来就把条目删了：先变墓碑，等回执后再发删除。
        todos.remove(0);
        align(&todos, &mut meta);
        assert_eq!(meta.tombstones.len(), 1);
        assert_eq!(meta.tombstones[0].kind, "update");
        assert!(plan_ops(&todos, &mut meta).is_empty(), "在途更新未回执，不能发删除");

        apply_acks(
            &mut meta,
            &[Ack {
                op_id: ops[0].op_id.clone(),
                status: "applied".into(),
                rev: Some(3),
                seq: Some(3),
            }],
        );
        let ops = plan_ops(&todos, &mut meta);
        assert_eq!(ops.len(), 1);
        assert!(ops[0].deleted, "回执后应补发删除");
        assert_eq!(ops[0].base_rev, 3);
    }

    #[test]
    fn apply_records_upserts_inserts_and_deletes() {
        let mut todos = TodoList {
            items: vec![item("A"), item("B")],
        };
        let mut meta = Meta::default();
        align(&todos.items, &mut meta);
        let id_a = meta.items[0].id.clone();

        let record = Record {
            id: id_a.clone(),
            kind: "todo_item".into(),
            rev: 5,
            data: serde_json::json!({"text":"A 改","sort":0}),
            ..Default::default()
        };
        let (changed, failed) = apply_records(&mut todos, &mut meta, &[record]);
        assert!(changed && failed.is_empty());
        assert_eq!(todos.items[0].text, "A 改");
        assert_eq!(meta.items[0].rev, 5);

        let record = Record {
            id: "remote-new".into(),
            kind: "todo_item".into(),
            rev: 1,
            data: serde_json::json!({"text":"远端新增","sort":100}),
            ..Default::default()
        };
        apply_records(&mut todos, &mut meta, &[record]);
        assert_eq!(todos.items.len(), 3);
        assert_eq!(todos.items[1].text, "远端新增");

        let record = Record {
            id: "remote-new".into(),
            kind: "todo_item".into(),
            rev: 2,
            deleted: true,
            ..Default::default()
        };
        apply_records(&mut todos, &mut meta, &[record]);
        assert_eq!(todos.items.len(), 2);
        assert!(!meta.items.iter().any(|entry| entry.id == "remote-new"));

        let broken = Record {
            id: "bad".into(),
            kind: "todo_item".into(),
            rev: 1,
            data: serde_json::json!({"text": 123}),
            ..Default::default()
        };
        let (_, failed) = apply_records(&mut todos, &mut meta, &[broken]);
        assert_eq!(failed.len(), 1, "坏数据留在 inbox，不能静默丢");
    }

    #[test]
    fn conflict_ack_unlocks_without_losing_the_pending_update() {
        let todos = vec![item("A")];
        let mut meta = Meta::default();
        align(&todos, &mut meta);
        meta.items[0].rev = 1;
        meta.items[0].hash = "old".into();
        let op = Op::new("todo_item", &meta.items[0].id, 1, "", serde_json::json!({"text":"A2"}));
        meta.items[0].pending = op.op_id.clone();
        apply_acks(
            &mut meta,
            &[Ack {
                op_id: op.op_id.clone(),
                status: "conflict".into(),
                rev: Some(2),
                seq: Some(4),
            }],
        );
        assert!(meta.items[0].pending.is_empty(), "冲突后要解锁");
        assert_eq!(meta.items[0].rev, 1, "rev 等服务端记录回灌时再更新");
    }
}
