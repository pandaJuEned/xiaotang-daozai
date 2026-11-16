
#[test]
fn nos_courses_join_the_calendar_and_the_resident_bar_shows_the_next_one() {
    use super::{ENV_LOCK, H};
    let _g=ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let mut h=H::new();
    // 用「明天的固定时刻」造课：无论测试何时跑，date 与 start_time 都自洽，
    // 且一定在未来，下一节课的判定不会因跨天而失真。
    let tomorrow=(chrono::Local::now().date_naive()+chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    let event=|uid:&str,title:&str,start:&str,end:&str|crate::nos::Event{
        uid:uid.into(),kind:"course".into(),title:title.into(),date:tomorrow.clone(),
        start_time:start.into(),end_time:end.into(),all_day:false,
        location:"教三 402".into(),color:"#A8C0B8".into(),teacher:"张三".into(),
        course_id:uid.into(),semester_id:"s1".into(),week:Some(11),status:String::new()};
    let agenda=crate::nos::Agenda{
        semesters:vec![crate::nos::Semester{id:"s1".into(),name:"2026 春季".into(),week_start_day:"monday".into()}],
        events:vec![event("c1","信号与系统","09:00","09:40"),event("c2","数字逻辑","14:00","14:45")],at:0};
    h.app.set_nos_agenda(agenda);
    assert_eq!(h.app.nos_events.len(),2,"课表事件应转成统一事件模型");
    let tomorrow_date=chrono::NaiveDate::parse_from_str(&tomorrow,"%Y-%m-%d").unwrap();
    let entries=h.app.calendar_entries(tomorrow_date);
    let nos_entries=entries.iter().filter(|entry|entry.sources.contains(&crate::calendar_ui::Source::Nos)).count();
    assert_eq!(nos_entries,2,"课表应并入日程页");
    let next=h.app.nos_next_course(chrono::Local::now().timestamp());
    assert!(next.is_some(),"应能找出下一节课");
    assert_eq!(next.map(|e|e.title.as_str()),Some("信号与系统"));
    // 常驻栏：开启后应占一个槽位；关闭后不占。
    h.app.settings.show_course=true;
    h.frame(vec![]);
    assert!(h.app.resident.current().x[crate::bar_layout::COURSE].is_some(),"开启后课表槽应就位");
    h.app.settings.show_course=false;
    h.frame(vec![]);
    assert!(h.app.resident.current().x[crate::bar_layout::COURSE].is_none(),"关闭后课表槽应消失");
}

#[test]
fn nos_meeting_conversion_handles_all_day_and_missing_end() {
    use super::ENV_LOCK;
    let _g=ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let all_day=crate::nos::Event{uid:"m1".into(),kind:"meeting".into(),title:"导师约谈".into(),date:"2026-11-14".into(),
        start_time:String::new(),end_time:String::new(),all_day:true,location:"行政楼".into(),color:String::new(),teacher:String::new(),
        course_id:String::new(),semester_id:String::new(),week:None,status:String::new()};
    let meeting=crate::calendar_ui::nos_meeting(&all_day).expect("全天事件也应可转换");
    assert!(meeting.all_day);
    assert_eq!(meeting.end,None,"没有结束时间就不推测时长");
    let timed=crate::nos::Event{start_time:"08:00".into(),end_time:"09:40".into(),all_day:false,..all_day.clone()};
    let meeting=crate::calendar_ui::nos_meeting(&timed).expect("定时事件应可转换");
    assert!(!meeting.all_day);
    assert_eq!(meeting.at,"08:00");
    assert!(meeting.end.is_some_and(|end|end-meeting.start==100*60),"08:00-09:40 应为 100 分钟");
}

#[test]
fn nos_todos_join_the_todo_page_read_only_without_touching_local_todos() {
    use super::{ENV_LOCK, H};
    let _g=ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let mut h=H::new();
    let event=|uid:&str,title:&str,date:&str,time:&str,status:&str|crate::nos::Event{
        uid:uid.into(),kind:"todo".into(),title:title.into(),date:date.into(),
        start_time:time.into(),end_time:String::new(),all_day:time.is_empty(),
        location:"说明".into(),color:String::new(),teacher:String::new(),
        course_id:String::new(),semester_id:String::new(),week:None,status:status.into()};
    let agenda=crate::nos::Agenda{
        semesters:vec![],
        events:vec![
            event("t1","写开题报告","2026-11-17","09:30","pending"),
            event("t2","交材料","2026-11-18","","done"),
            // 会议仍走原来的日程链路，不混进待办。
            event("m1","组会","2026-11-17","14:00",""),
        ],
        at:0};
    // 先把会议 kind 改回去（上面的闭包统一造 todo，单独修正这一条）。
    let mut events=agenda.events;
    events[2].kind="meeting".into();
    h.app.set_nos_agenda(crate::nos::Agenda{events,semesters:agenda.semesters,at:agenda.at});
    assert_eq!(h.app.nos_todos.len(),2,"只有 todo 事件进待办页");
    assert_eq!(h.app.nos_todos[0].text,"写开题报告");
    assert_eq!(h.app.nos_todos[0].due.as_deref(),Some("2026-11-17 09:30"));
    assert!(h.app.nos_todos[1].done_for(&chrono::Local::now().date_naive().format("%Y-%m-%d").to_string()),
        "NOS 已完成的待办在岛上标记为完成");
    // 本机待办不受影响：set_nos_agenda 前后条数不变，NOS 项不写回 todo.txt。
    assert!(h.app.todos.items.is_empty(),"NOS 待办不能写回本机清单");
    // 展开岛体切到待办页：NOS 待办的标题与 NOS 标签必须真的画出来。
    h.app.show_page(4);
    h.hover(H::local(300.0,60.0),0.8);
    let contains=|needle:&str|h.shapes.iter().any(|shape|matches!(&shape.shape,
        eframe::egui::Shape::Text(text) if text.galley.text().contains(needle)));
    assert!(contains("写开题报告"),"待办页应画出 NOS 待办标题");
    assert!(contains("交材料"),"第二条 NOS 待办也应可见");
    assert!(contains("NOS"),"NOS 待办应带来源标签");
}

#[test]
fn nos_event_status_helpers_and_wire_format() {
    let event=crate::nos::Event{
        uid:"t".into(),kind:"todo".into(),title:"任务".into(),date:"2026-11-17".into(),
        start_time:"09:30".into(),end_time:String::new(),all_day:false,
        location:String::new(),color:String::new(),teacher:String::new(),
        course_id:String::new(),semester_id:String::new(),week:None,status:"pending".into()};
    assert!(event.is_todo());
    assert!(!event.todo_done());
    let done=crate::nos::Event{status:"done".into(),..event.clone()};
    assert!(done.todo_done());
    let legacy=crate::nos::Event{status:String::new(),..event.clone()};
    assert!(!legacy.todo_done(),"老服务端没有 status 时按未完成处理");
    let course=crate::nos::Event{kind:"course".into(),..event};
    assert!(!course.is_todo());
}

#[test]
fn local_nos_server_settings_survive_a_round_trip_and_the_page_renders() {
    use super::{ENV_LOCK, H};
    let _g=ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let mut h=H::new();
    h.app.settings.nos_server_enabled=true;
    h.app.settings.nos_server_auto_start=true;
    h.app.settings.nos_server_dir="E:/dev/NOS/web".into();
    h.app.settings.nos_server_python="E:/dev/NOS/web/venv/Scripts/python.exe".into();
    h.app.settings.nos_server_host="127.0.0.1".into();
    h.app.settings.nos_server_port=8123;
    h.app.settings.save();
    let loaded=crate::model::Settings::load();
    assert!(loaded.nos_server_enabled && loaded.nos_server_auto_start);
    assert_eq!(loaded.nos_server_dir,"E:/dev/NOS/web");
    assert_eq!(loaded.nos_server_port,8123);
    let cfg=h.app.nos_server_config();
    assert_eq!(cfg.base_url(),"http://127.0.0.1:8123");
    assert_eq!(cfg.dir(),std::path::PathBuf::from("E:/dev/NOS/web"));
    assert!(cfg.uvicorn_args().windows(2).any(|pair|pair[0]=="--port" && pair[1]=="8123"));
    // 开启本地服务后，「课表」页签会多出整块控制区，必须仍能正常栅格化。
    h.app.settings_tab=15;h.app.open_settings();h.ctx.set_embed_viewports(false);
    for _ in 0..3 {
        let _=h.ctx.run(eframe::egui::RawInput::default(),|ctx|h.app.settings_ui(ctx,false));
    }
}

#[test]
fn nos_remote_mode_keeps_the_manually_entered_server_address() {
    use super::{ENV_LOCK, H};
    let _g=ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let mut h=H::new();
    h.app.settings.nos_server_enabled=false;
    h.app.settings.nos_base_url="http://192.168.1.10:8000".into();
    assert_eq!(h.app.nos_server_config().base_url(),format!("http://127.0.0.1:{}",crate::nos_server::DEFAULT_PORT));
    assert_eq!(h.app.nos_config().base_url,"http://192.168.1.10:8000",
        "关闭本地服务时岛仔只是客户端，不该改写手填的服务器地址");
}

#[test]
fn nos_meetings_enter_the_calendar_but_todos_stay_on_the_todo_page(){
    use super::{ENV_LOCK,H};
    let _g=ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let mut h=H::new();
    let day=(chrono::Local::now().date_naive()+chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    let event=|uid:&str,title:&str,kind:&str,start:&str,end:&str|crate::nos::Event{
        uid:uid.into(),kind:kind.into(),title:title.into(),date:day.clone(),
        start_time:start.into(),end_time:end.into(),all_day:false,
        location:"会议室".into(),color:String::new(),teacher:String::new(),
        course_id:String::new(),semester_id:"s1".into(),week:Some(11),status:String::new()};
    let agenda=crate::nos::Agenda{
        semesters:vec![crate::nos::Semester{id:"s1".into(),name:"2026 秋季".into(),week_start_day:"sunday".into()}],
        events:vec![
            event("m1","组会","meeting","14:30","16:10"),
            event("t1","写周报","todo","09:30",""),
        ],at:0};
    h.app.set_nos_agenda(agenda);
    assert_eq!(h.app.nos_events.len(),1,"会议进日程，待办不能混进日程");
    assert_eq!(h.app.nos_todos.len(),1,"待办只进待办页");
    let date=chrono::NaiveDate::parse_from_str(&day,"%Y-%m-%d").unwrap();
    let titles:Vec<&str>=h.app.calendar_entries(date).iter()
        .filter(|entry|entry.sources.contains(&crate::calendar_ui::Source::Nos))
        .map(|entry|entry.event.title.as_str()).collect();
    assert!(titles.contains(&"组会"),"NOS 会议应出现在日程页：{titles:?}");
    assert!(!titles.contains(&"写周报"),"NOS 待办不该在日程页重复出现：{titles:?}");
}
