//! 设置页「待办」页签：数据操作与渲染回归。
use super::{ENV_LOCK, H};
use eframe::egui::{self, Pos2, Rect};

fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
    shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn settings_todo_edits_round_trip_through_todo_txt() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = H::new();
    h.app.today = "2026-10-28".into();

    assert!(h.app.todo_add("写开题报告"));
    assert!(h.app.todo_add("交材料"));
    // 与岛内「加一个」一致：新条目排在最前面。
    assert_eq!(h.app.todos.items[0].text, "交材料");
    let loaded = crate::model::TodoList::load();
    assert_eq!(loaded.items.len(), 2, "设置页新增的待办应真的写进 todo.txt");
    assert_eq!(loaded.items[0].text, "交材料");
    assert!(!h.app.todo_add("   "), "空内容不该生成一条待办");
    assert_eq!(h.app.todos.items.len(), 2);

    assert!(h.app.todo_rename(0, "交材料给导师"));
    assert_eq!(crate::model::TodoList::load().items[0].text, "交材料给导师");
    assert!(!h.app.todo_rename(0, "  "), "留空视为放弃修改");
    assert_eq!(h.app.todos.items[0].text, "交材料给导师");
    assert!(!h.app.todo_rename(9, "越界"), "越界改正文应安全返回");

    assert!(h.app.todo_toggle(0));
    assert!(h.app.todos.items[0].done_for("2026-10-28"));
    assert_eq!(
        crate::model::TodoList::load().items[0].done_on.as_deref(),
        Some("2026-10-28"),
        "勾选结果要落盘"
    );
    assert!(h.app.todo_toggle(0));
    assert!(!h.app.todos.items[0].done_for("2026-10-28"), "再点一次应取消完成");

    assert!(h.app.todo_remove(0));
    assert_eq!(crate::model::TodoList::load().items.len(), 1);
    assert!(!h.app.todo_remove(9), "越界删除应安全返回");
}

#[test]
fn settings_todo_reminder_validates_and_persists() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = H::new();
    h.app.today = "2026-10-28".into();
    h.app.todo_add("写周报");

    // 只写时刻时按今天算，和岛内待办页的输入口径一致。
    assert_eq!(h.app.todo_save_reminder(0, "09:30", 0), Ok(()));
    assert_eq!(h.app.todos.items[0].due.as_deref(), Some("2026-10-28 09:30"));
    assert_eq!(
        crate::model::TodoList::load().items[0].due.as_deref(),
        Some("2026-10-28 09:30"),
        "提醒要落盘"
    );

    assert!(h.app.todo_save_reminder(0, "明天中午", 0).is_err(), "非法时间应报错");
    assert_eq!(
        h.app.todos.items[0].due.as_deref(),
        Some("2026-10-28 09:30"),
        "校验失败不能改坏已有提醒"
    );

    assert_eq!(h.app.todo_save_reminder(0, "08:00", crate::model::REPEAT_WORKDAYS), Ok(()));
    assert_eq!(h.app.todos.items[0].repeat, crate::model::REPEAT_WORKDAYS);
    assert_eq!(
        crate::model::TodoList::load().items[0].repeat,
        crate::model::REPEAT_WORKDAYS,
        "每周重复要落盘"
    );

    assert_eq!(h.app.todo_save_reminder(0, "", 0), Ok(()));
    assert!(h.app.todos.items[0].due.is_none(), "清空时间等于取消提醒");
    assert!(h.app.todo_save_reminder(9, "09:30", 0).is_err(), "条目不在时应报错");
}

#[test]
fn settings_todo_tab_renders_editable_rows_and_nos_entries() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = H::new();
    h.app.today = "2026-10-28".into();
    h.app.todo_add("写开题报告");
    let mut nos = crate::model::TodoItem::new("写周报");
    nos.due = Some("2026-11-16 23:06".into());
    h.app.nos_todos = vec![nos];

    let mut raster = crate::preview_raster::Raster::new(640, 760);
    for _ in 0..4 {
        let out = h.ctx.run(
            egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(640.0, 760.0))), ..Default::default() },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| h.app.todo_settings(ui));
            },
        );
        raster.update_textures(&out.textures_delta);
        h.shapes = out.shapes;
    }
    let texts = texts(&h.shapes);
    assert!(texts.iter().any(|text| text == "写开题报告"), "本机待办正文应画出来");
    assert!(texts.iter().any(|text| text == "添加"), "应有新增入口");
    assert!(texts.iter().any(|text| text == "今天还有 1 项"), "进度应按今天计算");
    assert!(texts.iter().any(|text| text == "写周报"), "NOS 待办也应列出");
    assert!(texts.iter().any(|text| text == "NOS"), "NOS 行要带来源标签");
    assert!(
        texts.iter().any(|text| text.contains("只读展示")),
        "NOS 区要说明只读，避免用户在这里找编辑入口"
    );
}

#[test]
fn settings_window_todo_tab_and_reminder_window_run_in_the_child_viewport() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = H::new();
    h.app.today = "2026-10-28".into();
    h.app.todo_add("写开题报告");
    h.app.settings_tab = 4;
    h.app.open_settings();
    h.ctx.set_embed_viewports(false);
    for _ in 0..3 {
        let _ = h.ctx.run(egui::RawInput::default(), |ctx| h.app.settings_ui(ctx, false));
    }
    // 提醒小窗画在设置子视口里：打开后再走几帧，确认这条路径不会把状态吃掉。
    h.app.todo_reminder = Some(crate::todo_settings::ReminderDraft {
        index: 0,
        due: "2026-10-28 18:30".into(),
        repeat: crate::model::REPEAT_WORKDAYS,
        error: String::new(),
        focused: true,
    });
    for _ in 0..3 {
        let _ = h.ctx.run(egui::RawInput::default(), |ctx| h.app.settings_ui(ctx, false));
    }
    assert!(h.app.todo_reminder.is_some(), "设置窗口还开着，提醒小窗不该自己消失");
    // 关掉设置窗口后草稿要一并收起，下次打开不该残留一个旧弹窗。
    h.app.settings_open = false;
    let _ = h.ctx.run(egui::RawInput::default(), |ctx| h.app.settings_ui(ctx, false));
    assert!(h.app.todo_reminder.is_none(), "关闭设置窗口应清掉提醒草稿");
}

#[test]
#[ignore="按需导出设置页待办清单的预览图"]
fn preview_settings_todo_tab(){
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = H::new();
    h.app.today = "2026-10-28".into();
    h.app.todo_add("交材料给导师");
    h.app.todo_add("写开题报告");
    h.app.todo_add("每周例会准备");
    h.app.todos.items[0].due = Some("2026-10-28 18:30".into());
    h.app.todos.items[0].repeat = crate::model::REPEAT_WORKDAYS;
    h.app.todos.items[2].done_on = Some("2026-10-28".into());
    let mut nos = crate::model::TodoItem::new("写周报");
    nos.due = Some("2026-11-16 23:06".into());
    let mut nos2 = crate::model::TodoItem::new("交材料");
    nos2.due = Some("2026-11-18 13:05".into());
    h.app.nos_todos = vec![nos, nos2];
    let (w,hgt)=(640.0f32,700.0f32);
    let ppp=2.0f32;
    let mut raster = crate::preview_raster::Raster::new((w*ppp)as usize,(hgt*ppp)as usize);
    // 两套配色都出一张：设置页跟随系统/用户设置，浅色下的对比度也得过关。
    for (dark,file,bg,page) in [
        (true,"todo-settings-preview.png",[0.10,0.10,0.11,1.0],egui::Color32::from_rgb(25,25,27)),
        (false,"todo-settings-preview-light.png",[0.95,0.95,0.96,1.0],egui::Color32::from_rgb(246,247,249)),
    ] {
        h.ctx.set_visuals(if dark{egui::Visuals::dark()}else{egui::Visuals::light()});
        raster.buf.fill(bg);
        for _ in 0..4 {
            let out = h.ctx.run(
                egui::RawInput{screen_rect:Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(w,hgt))),..Default::default()},
                |ctx| { egui::CentralPanel::default().frame(egui::Frame::new().fill(page).inner_margin(egui::Margin::symmetric(22,16))).show(ctx,|ui|{
                    ui.set_width(w-44.0);
                    h.app.todo_settings(ui);
                });},
            );
            raster.update_textures(&out.textures_delta);
            raster.buf.fill(bg);
            raster.draw(&h.ctx.tessellate(out.shapes.clone(),ppp),ppp);
            let mut rgba=Vec::new();raster.to_rgba8(&mut rgba);
            let image=image::RgbaImage::from_raw((w*ppp)as u32,(hgt*ppp)as u32,rgba).unwrap();
            image.save(std::env::temp_dir().join(file)).unwrap();
            raster.free_textures(&out.textures_delta);
        }
        println!("{}", std::env::temp_dir().join(file).display());
    }
}

#[test]
#[ignore="按需导出提醒小窗口的预览图"]
fn preview_reminder_window(){
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = H::new();
    h.app.today = "2026-10-28".into();
    h.app.todo_add("交材料给导师");
    h.app.todo_reminder = Some(crate::todo_settings::ReminderDraft{
        index:0,
        due:"2026-10-28 18:30".into(),
        repeat:crate::model::REPEAT_WORKDAYS,
        error:String::new(),
        focused:true,
    });
    let (w,hgt)=(760.0f32,520.0f32);
    let ppp=2.0f32;
    let mut raster = crate::preview_raster::Raster::new((w*ppp)as usize,(hgt*ppp)as usize);
    for _ in 0..4 {
        let out = h.ctx.run(
            egui::RawInput{screen_rect:Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(w,hgt))),..Default::default()},
            |ctx| {
                egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::from_rgb(25,25,27))).show(ctx,|ui|{
                    ui.set_width(w-44.0);
                    h.app.todo_settings(ui);
                });
                h.app.todo_reminder_window(ctx);
            },
        );
        raster.update_textures(&out.textures_delta);
        raster.buf.fill([0.10,0.10,0.11,1.0]);
        raster.draw(&h.ctx.tessellate(out.shapes.clone(),ppp),ppp);
        let mut rgba=Vec::new();raster.to_rgba8(&mut rgba);
        let image=image::RgbaImage::from_raw((w*ppp)as u32,(hgt*ppp)as u32,rgba).unwrap();
        image.save(std::env::temp_dir().join("todo-reminder-preview.png")).unwrap();
        raster.free_textures(&out.textures_delta);
    }
    println!("{}", std::env::temp_dir().join("todo-reminder-preview.png").display());
}
