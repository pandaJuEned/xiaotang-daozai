
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
        course_id:uid.into(),semester_id:"s1".into(),week:Some(11)};
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
        course_id:String::new(),semester_id:String::new(),week:None};
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
