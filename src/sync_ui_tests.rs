#[test]
fn sync_and_devices_tabs_render_without_panicking() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.settings.sync_base_url = "https://nos.example.com".into();
    h.app.sync = crate::sync::Snapshot {
        configured: true,
        connected: true,
        user: "姚".into(),
        cursor: 12,
        pending: 2,
        inbox: 1,
        last_ok: chrono::Local::now().timestamp(),
        devices: vec![
            crate::sync::DeviceView {
                id: "dev-home".into(),
                name: "书房台式".into(),
                online: true,
                platform: "windows".into(),
                last_seen: "2026-11-18T14:50:40.262Z".into(),
            },
            crate::sync::DeviceView {
                id: "dev-laptop".into(),
                name: "宿舍笔记本".into(),
                online: false,
                platform: "windows".into(),
                last_seen: "2026-11-18T10:00:00.000Z".into(),
            },
        ],
        ..Default::default()
    };
    h.app.settings_tab = 16;
    h.app.open_settings();
    h.ctx.set_embed_viewports(false);
    for _ in 0..3 {
        let _ = h
            .ctx
            .run(eframe::egui::RawInput::default(), |ctx| h.app.settings_ui(ctx, false));
    }
}

#[test]
fn sync_pump_uploads_only_after_first_pull_and_applies_remote_todos() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.todos.items = vec![
        crate::model::TodoItem { text: "刷牙".into(), ..Default::default() },
        crate::model::TodoItem { text: "交报告".into(), ..Default::default() },
    ];

    // 首次拉取完成前不上传，避免第二台设备重复创建。
    h.app.sync.ready = false;
    crate::sync_map::pump(&mut h.app);
    assert!(crate::sync::pending_ops().is_empty(), "未 ready 时不应入队");

    h.app.sync.ready = true;
    crate::sync_map::pump(&mut h.app);
    let todo_ops = |list: Vec<crate::sync::Op>| -> Vec<crate::sync::Op> {
        list.into_iter().filter(|op| op.kind == "todo_item").collect()
    };
    assert_eq!(todo_ops(crate::sync::pending_ops()).len(), 2, "两条本地待办应生成新建操作");
    crate::sync_map::pump(&mut h.app);
    assert_eq!(todo_ops(crate::sync::pending_ops()).len(), 2, "没变化不应重复入队");

    // 远端新增一条：pump 应把它插到 sort 指定的位置。
    crate::sync::append_inbox(&[crate::sync::Record {
        id: "remote-1".into(),
        kind: "todo_item".into(),
        rev: 1,
        data: serde_json::json!({"text":"远端新增","sort":100}),
        ..Default::default()
    }])
    .unwrap();
    crate::sync_map::pump(&mut h.app);
    assert_eq!(h.app.todos.items.len(), 3);
    assert_eq!(h.app.todos.items[1].text, "远端新增");
}

#[test]
fn sync_pump_migrates_water_and_projects_remote_sips() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    let mut hours = [0u32; 24];
    hours[9] = 250;
    hours[14] = 250;
    h.app.water = crate::model::Water {
        drunk_ml: 500,
        hours,
        start_day: today.clone(),
        ..Default::default()
    };

    // 首次接入：本地 500 拆成两条 sip 上传，投影仍是 500。
    crate::sync_map::pump(&mut h.app);
    let water_ops: Vec<_> = crate::sync::pending_ops()
        .into_iter()
        .filter(|op| op.kind == "water_sip")
        .collect();
    assert_eq!(water_ops.len(), 2, "应按小时迁移出两条 sip");
    assert_eq!(h.app.water.drunk_ml, 500);
    assert_eq!(h.app.water.hours[9], 250);

    // 另一台又喝 250：远端记录进 inbox，投影应涨到 750。
    crate::sync::append_inbox(&[crate::sync::Record {
        id: "remote-water".into(),
        kind: "water_sip".into(),
        day: today.clone(),
        rev: 1,
        data: serde_json::json!({"ml":250,"hour":15,"at":1,"day":today}),
        ..Default::default()
    }])
    .unwrap();
    crate::sync_map::pump(&mut h.app);
    assert_eq!(h.app.water.drunk_ml, 750);
    assert_eq!(h.app.water.hours[15], 250);
}

#[test]
fn sync_pump_publishes_focus_session_once() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    let now = chrono::Local::now().timestamp();
    h.app.pomo.session_id = "focus-local".into();
    h.app.pomo.started_at = now;
    h.app.pomo.duration = 1500;
    h.app.pomo.device = "dev-a".into();
    h.app.pomo.remote = false;
    h.app.pomo.phase = crate::app::Phase::Focus;
    h.app.pomo.remaining = 1500.0;
    h.app.pomo.running = true;
    h.app.pomo.deadline = Some(now + 1500);
    h.app.pomo.state = "running".into();

    crate::sync_map::pump(&mut h.app);
    crate::sync_map::pump(&mut h.app);
    let ops: Vec<_> = crate::sync::pending_ops()
        .into_iter()
        .filter(|op| op.kind == "focus_session")
        .collect();
    assert_eq!(ops.len(), 1, "同一次会话只能入队一条，重复泵不能重复上传");
    assert_eq!(ops[0].base_rev, 0);
    assert_eq!(ops[0].data["state"], "running");
}

#[test]
fn remote_running_focus_is_adopted_without_echo() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    let now = chrono::Local::now().timestamp();
    crate::sync::append_inbox(&[crate::sync::Record {
        id: "remote-focus".into(),
        kind: "focus_session".into(),
        rev: 3,
        data: serde_json::json!({
            "phase":"focus","state":"running","started_at":now-120,
            "end_at":now+600,"duration":900,"remaining":720,
            "device":"dev-b","completed":false
        }),
        ..Default::default()
    }])
    .unwrap();
    crate::sync_map::pump(&mut h.app);
    assert_eq!(h.app.pomo.session_id, "remote-focus");
    assert!(h.app.pomo.remote && h.app.pomo.running, "应接管为远程运行中");
    assert!(h.app.pomo.remaining > 500.0);
    let echoes: Vec<_> = crate::sync::pending_ops()
        .into_iter()
        .filter(|op| op.kind == "focus_session")
        .collect();
    assert!(echoes.is_empty(), "远程会话不能回声成自己的 op");
}

#[test]
fn remote_finished_focus_joins_history() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    let now = chrono::Local::now().timestamp();
    crate::sync::append_inbox(&[crate::sync::Record {
        id: "done-remote".into(),
        kind: "focus_session".into(),
        rev: 2,
        data: serde_json::json!({
            "phase":"focus","state":"done","started_at":now-1500,
            "end_at":now,"duration":1500,"remaining":0,
            "device":"dev-b","completed":true
        }),
        ..Default::default()
    }])
    .unwrap();
    crate::sync_map::pump(&mut h.app);
    assert!(h.app.focus_history.has("done-remote"), "远端完成应进历史");
}

#[test]
fn sync_settings_roundtrip_keeps_primary_device() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.settings.sync_base_url = "https://nos.example.com".into();
    h.app.settings.sync_primary_device = "dev-laptop".into();
    h.app.settings.save();

    let loaded = crate::model::Settings::load();
    assert!(loaded.sync_enabled, "同步开关应落盘");
    assert_eq!(loaded.sync_base_url, "https://nos.example.com");
    assert_eq!(loaded.sync_primary_device, "dev-laptop");
}

#[test]
fn deadline_timer_finishes_even_after_a_long_sleep() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    let now = chrono::Local::now().timestamp();
    h.app.pomo.phase = crate::app::Phase::Focus;
    h.app.pomo.running = true;
    h.app.pomo.remaining = 1500.0;
    // 模拟睡眠 10 分钟后醒来：remaining 还停在 1500，但 deadline 已过。
    h.app.pomo.deadline = Some(now - 600);
    h.frame(vec![]);
    assert!(!h.app.pomo.running, "deadline 已过应立即结束");
    assert_eq!(h.app.pomo.phase, crate::app::Phase::Rest);
    assert!(h.app.pomo.remaining > 0.0);
}

#[test]
fn finished_session_publishes_the_phase_that_ended() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    let now = chrono::Local::now().timestamp();
    h.app.pomo.session_id = "focus-done".into();
    h.app.pomo.started_at = now - 1500;
    h.app.pomo.duration = 1500;
    h.app.pomo.device = "dev-a".into();
    h.app.pomo.remote = false;
    h.app.pomo.phase = crate::app::Phase::Focus;
    h.app.pomo.remaining = 1.0;
    h.app.pomo.running = true;
    h.app.pomo.deadline = Some(now - 1);
    h.app.pomo.state = "running".into();

    h.frame(vec![]);
    assert_eq!(h.app.pomo.phase, crate::app::Phase::Rest);
    crate::sync_map::pump(&mut h.app);
    let ops: Vec<_> = crate::sync::pending_ops()
        .into_iter()
        .filter(|op| op.kind == "focus_session")
        .collect();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].data["state"], "done");
    assert_eq!(ops[0].data["phase"], "focus", "done 不能记成已经切过去的 rest");
    assert_eq!(ops[0].data["duration"], 1500);
}

#[test]
fn sync_pump_merges_local_calendar_events() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    let now = chrono::Local::now().timestamp();
    h.app.calendar.local = vec![crate::local_calendar::Entry {
        event: crate::mail::Meeting {
            uid: "local-1".into(),
            title: "自己的安排".into(),
            start: now + 3600,
            end: Some(now + 7200),
            all_day: false,
            at: String::new(),
        },
        reminder: Some(15),
    }];

    crate::sync_map::pump(&mut h.app);
    let ops: Vec<_> = crate::sync::pending_ops()
        .into_iter()
        .filter(|op| op.kind == "local_event")
        .collect();
    assert_eq!(ops.len(), 1, "本地独有日程应上传");
    assert_eq!(ops[0].id, "local-1");

    crate::sync::append_inbox(&[crate::sync::Record {
        id: "remote-1".into(),
        kind: "local_event".into(),
        rev: 1,
        data: serde_json::json!({
            "title":"另一台的安排","start":now+10800,"end":now+14400,
            "all_day":false,"reminder":10
        }),
        ..Default::default()
    }])
    .unwrap();
    crate::sync_map::pump(&mut h.app);
    assert_eq!(h.app.calendar.local.len(), 2);
    let saved = crate::local_calendar::load().unwrap();
    assert!(saved.iter().any(|entry| entry.event.uid == "remote-1"));
}

#[test]
fn sync_pump_applies_account_settings_per_key() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings.sync_enabled = true;
    h.app.sync.ready = true;
    h.app.settings.focus_minutes = 50;

    crate::sync_map::pump(&mut h.app);
    let focus = crate::sync::pending_ops()
        .into_iter()
        .find(|op| op.id == "setting-focus_minutes");
    let focus = focus.expect("修改过的设置键应上传");
    assert_eq!(focus.data["value"], 50);

    crate::sync::append_inbox(&[crate::sync::Record {
        id: "setting-water_minutes".into(),
        kind: "setting".into(),
        rev: 4,
        data: serde_json::json!({"key":"water_minutes","value":30}),
        ..Default::default()
    }])
    .unwrap();
    crate::sync_map::pump(&mut h.app);
    assert_eq!(h.app.settings.water_minutes, 30);
}

#[test]
fn about_page_renders_with_the_project_credit() {
    use super::{ENV_LOCK, H};
    let _g = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut h = H::new();
    h.app.settings_tab = 9;
    h.app.open_settings();
    h.ctx.set_embed_viewports(false);
    for _ in 0..3 {
        let _ = h
            .ctx
            .run(eframe::egui::RawInput::default(), |ctx| h.app.settings_ui(ctx, false));
    }
}
