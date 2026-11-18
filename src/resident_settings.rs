use crate::{bar_layout::*,monitor::MonitorSnapshot,paint::{self,Card,H},theme::{self,W}};
use eframe::egui::{self,Color32,RichText,Rect,Pos2,Vec2};

pub(crate) fn preview_layout(order:&[usize],quotas:usize,weather:bool,status:StatusSlots)->Layout {
    let mut layout=Layout::with_status_slots(true,quotas.max(1),true,true,false,true,weather,status);
    layout.reorder(order);layout
}

/// 状态组件是否已有可显示的数据。`placeholder=true` 时只按开关决定是否预览占位，
/// 这样设置页能直接看到每个已开启组件排在什么位置。
fn status_enabled(settings:&crate::model::Settings,slot:usize)->bool {
    match slot {
        NETSPEED=>settings.show_net_speed,
        COURSE=>settings.show_course,
        CALENDAR=>settings.show_calendar_status,
        CPU_INFO=>settings.show_cpu,
        GPU_INFO=>settings.show_gpu,
        MEMORY_INFO=>settings.show_memory,
        DISK_INFO=>settings.show_disk,
        TEMPERATURE=>settings.show_temperature,
        _=>false,
    }
}

fn percent_label(value:Option<f32>,placeholder:bool)->String {
    match value {
        Some(value)=>format!("{:.0}%",value.clamp(0.0,100.0)),
        None if placeholder=>"--".into(),
        None=>String::new(),
    }
}

impl crate::app::App {
    /// 把设置开关和实际数据合并成布局用的末尾槽；预览模式只先看开关。
    pub(crate) fn resident_status_slots(&self,monitor:Option<&MonitorSnapshot>,now:i64,placeholder:bool)->StatusSlots {
        let available=|slot:usize|match slot {
            NETSPEED=>monitor.is_some(),
            COURSE=>self.nos_next_course(now).is_some(),
            CALENDAR=>self.current_calendar_event(now).is_some(),
            CPU_INFO=>monitor.is_some_and(|m|m.cpu_percent.is_some()),
            GPU_INFO=>monitor.is_some_and(|m|m.gpu_percent.is_some()),
            MEMORY_INFO=>monitor.is_some_and(|m|m.mem_percent().is_some()),
            DISK_INFO=>monitor.is_some_and(|m|m.disk_percent.is_some()),
            TEMPERATURE=>monitor.is_some_and(|m|m.hottest_temp().is_some()),
            _=>false,
        };
        let enabled=|slot:usize|status_enabled(&self.settings,slot)&&(placeholder||available(slot));
        StatusSlots {net:enabled(NETSPEED),course:enabled(COURSE),calendar:enabled(CALENDAR),cpu:enabled(CPU_INFO),gpu:enabled(GPU_INFO),memory:enabled(MEMORY_INFO),disk:enabled(DISK_INFO),temperature:enabled(TEMPERATURE)}
    }

    /// 末尾状态组件的紧凑文案；`placeholder=true` 时即使没有数据也给出占位文字。
    pub(crate) fn resident_status_text(&self,slot:usize,monitor:Option<&MonitorSnapshot>,now:i64,placeholder:bool)->String {
        match slot {
            NETSPEED=>match monitor {
                Some(m)=>{
                    let mut style=self.monitor_cfg.display;
                    style.short_mode=true;style.space=false;
                    format!("↓{} ↑{}",crate::monitor::format_speed(m.down_bps,&style),crate::monitor::format_speed(m.up_bps,&style))
                },
                None if placeholder=>"↓-- ↑--".into(),
                None=>String::new(),
            },
            COURSE=>match self.nos_next_course(now) {
                Some(event)=>{
                    let clock=crate::nos::clock_label(&event.start_time);
                    if clock.is_empty(){event.title.clone()}else{format!("{clock} {}",event.title)}
                },
                None if placeholder=>"--:-- --".into(),
                None=>String::new(),
            },
            CALENDAR=>match self.current_calendar_event(now) {
                Some(event)=>format!("进行中 · {}",event.title),
                None if placeholder=>"进行中 · --".into(),
                None=>String::new(),
            },
            CPU_INFO=>{let value=percent_label(monitor.and_then(|m|m.cpu_percent),placeholder);if value.is_empty(){value}else{format!("CPU {value}")}},
            GPU_INFO=>{let value=percent_label(monitor.and_then(|m|m.gpu_percent),placeholder);if value.is_empty(){value}else{format!("GPU {value}")}},
            MEMORY_INFO=>{let value=percent_label(monitor.and_then(|m|m.mem_percent()),placeholder);if value.is_empty(){value}else{format!("内存 {value}")}},
            DISK_INFO=>{let value=percent_label(monitor.and_then(|m|m.disk_percent),placeholder);if value.is_empty(){value}else{format!("硬盘 {value}")}},
            TEMPERATURE=>match monitor.and_then(|m|m.hottest_temp()) {
                Some(value)=>format!("{value:.0}°C"),
                None if placeholder=>"--°C".into(),
                None=>String::new(),
            },
            _=>String::new(),
        }
    }

    fn resident_status_data_available(&self,slot:usize,monitor:Option<&MonitorSnapshot>,now:i64)->bool {
        match slot {
            NETSPEED=>monitor.is_some(),
            COURSE=>self.nos_next_course(now).is_some(),
            CALENDAR=>self.current_calendar_event(now).is_some(),
            CPU_INFO=>monitor.is_some_and(|m|m.cpu_percent.is_some()),
            GPU_INFO=>monitor.is_some_and(|m|m.gpu_percent.is_some()),
            MEMORY_INFO=>monitor.is_some_and(|m|m.mem_percent().is_some()),
            DISK_INFO=>monitor.is_some_and(|m|m.disk_percent.is_some()),
            TEMPERATURE=>monitor.is_some_and(|m|m.hottest_temp().is_some()),
            _=>false,
        }
    }
}

impl crate::app::App {
    pub(crate) fn resident_order_settings(&mut self,ui:&mut egui::Ui){
        let default=if self.settings.show_battery {DEFAULT_ORDER.to_vec()}else{vec![QUOTA,PET,CLOCK,BATTERY,MUSIC,FOCUS]};
        let mut order=if valid_order(&self.settings.resident_order){self.settings.resident_order.clone()}else{default.clone()};
        ui.horizontal(|ui|{
            ui.label(RichText::new("拖动岛上的图标").color(ui.visuals().text_color()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui|{
                if ui.small_button("恢复默认").clicked(){self.settings.resident_order.clear();order=default;}
            });
        });
        ui.add_space(12.0);
        let indices=self.quota_bar_indices();let count=indices.len();
        let weather_snapshot=self.weather_pill().cloned();
        let now=chrono::Local::now().timestamp();
        let status=self.resident_status_slots(self.monitor_last.as_ref(),now,true);
        let mut layout=preview_layout(&order,count,weather_snapshot.is_some(),status);
        for (position,extra) in self.quota_balance_layout(ui.ctx()){layout.balance_tail(position,extra);}
        if self.settings.clock_period!=0 { layout.widen_clock(40.0); }
        if let Some(extra)=self.weather_pill_extra(ui.ctx()){layout.widen(WEATHER,extra);}
        let (stage,_)=ui.allocate_exact_size(Vec2::new(ui.available_width(),100.0),egui::Sense::hover());
        ui.painter().rect_filled(stage,10.0,Color32::from_rgb(19,25,36));
        let scale=((stage.width()-28.0)/layout.width).min(1.0);
        let origin=Pos2::new(stage.center().x-layout.width*scale/2.0,stage.top());
        // 预览跟随当前的皮肤与形状。
        let pill=self.settings.island_shape==crate::model::IslandShape::Pill;
        let origin=if pill{origin+Vec2::new(0.0,4.0)}else{origin};
        let body=Rect::from_min_size(origin,Vec2::new(layout.width*scale,30.0*scale));
        let fill=self.preview_fill();
        if pill {ui.painter().rect_filled(body,15.0*scale,fill);}
        else {crate::shape::fill(ui.painter(),body,8.0*scale,10.0*scale,12.0*scale,fill,1.0/ui.ctx().pixels_per_point());}
        if self.settings.skin.is_glass() {
            let rim=egui::Stroke::new(1.0_f32,Color32::from_white_alpha(60));
            if pill {ui.painter().rect_stroke(body,15.0*scale,rim,egui::StrokeKind::Inside);}
            else {
                let edge=Rect::from_min_max(body.min+Vec2::new(0.5,0.0),body.max-Vec2::new(0.5,0.5));
                ui.painter().add(egui::Shape::line(crate::shape::outline(edge,8.0*scale,10.0*scale,12.0*scale-0.5),rim));
            }
        }
        let content_from=crate::skin_paint::shape_count(ui.ctx(),ui.layer_id());
        let mut card=Card::new(ui,origin,stage);card.s=scale;
        let cx=layout.width/2.0;
        let pointer=ui.ctx().pointer_interact_pos();
        let dragged=egui::DragAndDrop::payload::<usize>(ui.ctx());
        let mut movement=None;
        let label=|slot|match slot{CLOCK=>"时间",BATTERY=>"电量",QUOTA=>"AI 额度",MUSIC=>"音乐",FOCUS=>"专注",_=>"吉祥物"};
        for (index,&slot) in order.iter().enumerate(){
            let x=cx+layout.x[slot].unwrap();
            let width=match slot{CLOCK=>if self.settings.clock_period!=0 {84.0}else{44.0},BATTERY=>20.0,PET=>32.0,MUSIC=>14.0,FOCUS=>crate::focus_bar::WIDTH,_=>{
                let left=layout.quota_slots().filter_map(|i|layout.x[i].map(|x|x-layout.widths[i]/2.0)).fold(f32::INFINITY,f32::min);
                let right=layout.quota_slots().filter_map(|i|layout.x[i].map(|x|x+layout.widths[i]/2.0)).fold(f32::NEG_INFINITY,f32::max);
                right-left
            }};
            let left=if slot==QUOTA{cx+layout.quota_slots().filter_map(|i|layout.x[i].map(|x|x-layout.widths[i]/2.0)).fold(f32::INFINITY,f32::min)}else{x-width/2.0};
            // 天气与时钟成组，拖拽热区一并盖住，拖时钟会带着天气走。
            let (left,width)=if slot==CLOCK&&layout.x[WEATHER].is_some(){(left,width+12.0+layout.widths[WEATHER])}else{(left,width)};
            let rect=card.rect(left-5.0,1.0,width+10.0,28.0);
            let response=ui.interact(rect,ui.id().with(("resident-icon",slot)),egui::Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::Grab);
            response.dnd_set_drag_payload(slot);
            if let (Some(from),Some(point))=(dragged.as_deref(),pointer){
                if *from!=slot && rect.contains(point) && ui.input(|i|i.pointer.primary_down()){movement=Some((*from,index));}
            }
            let is_dragged=dragged.as_deref()==Some(&slot);
            let dx=if is_dragged{pointer.map(|p|((p.x-origin.x)/scale-(left+width/2.0)).clamp(-left,layout.width-left-width)).unwrap_or(0.0)}else{0.0};
            if is_dragged {ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);}
            else if response.hovered(){ui.painter().rect_filled(rect,6.0,Color32::from_white_alpha(18));}
            let active=match slot{BATTERY=>self.settings.show_battery&&self.resident_power.0.is_some(),PET=>self.settings.pet,QUOTA=>self.settings.show_quota&&count>0,MUSIC=>self.media.is_music(),FOCUS=>self.pomo.running,_=>true};
            let c=card.shifted(dx,0.0).faded(if active||is_dragged{1.0}else{0.38}).ghost();
            match slot{
                BATTERY=>paint::battery(&c,x,15.0,self.resident_power.0,self.resident_power.1,1.0),
                CLOCK=>{
                    let text=self.settings.clock_text(&chrono::Local::now().format("%H:%M:%S").to_string());
                    paint::clock(&c,x-paint::clock_width(&c,&text,theme::size::PILL_CLOCK)/2.0,21.0,&text,theme::size::PILL_CLOCK);
                    if let Some(snapshot)=weather_snapshot.as_ref() {
                        self.paint_pill_weather(&c,cx+layout.x[WEATHER].unwrap(),&layout,snapshot);
                    }
                },
                PET=>self.pet.draw(&c,x,self.t,self.skin_palette()),
                MUSIC=>paint::playback_bars(&c,x,15.0,self.t,self.media.playing,theme::WHITE),
                FOCUS=>crate::focus_bar::draw(&c,x,15.0,self.t,&self.pomo,self.phase_secs(self.pomo.phase),false),
                QUOTA if count>0=>self.draw_quota_bar(&c,cx,&layout),
                _=>{c.ring(x,15.0,10.0,2.0,theme::GRAY);c.text(x,18.0,"AI",7.0,W::Bold,theme::WHITE,H::Center);},
            }
            if is_dragged {ui.painter().line_segment([Pos2::new(rect.center().x,body.bottom()+6.0),Pos2::new(rect.center().x,body.bottom()+12.0)],egui::Stroke::new(2.0,theme::BLUE));}
            response.on_hover_text(label(slot));
        }
        // 末尾状态槽与设置页「状态组件」一一对应，按列表顺序展示；无数据时用灰色占位。
        let tail_label=|slot|match slot{NETSPEED=>"网速",COURSE=>"下一节课",CALENDAR=>"进行中日程",CPU_INFO=>"CPU 信息",GPU_INFO=>"GPU 信息",MEMORY_INFO=>"内存信息",DISK_INFO=>"硬盘信息",TEMPERATURE=>"温度",_=>"状态组件"};
        for &slot in STATUS_TAIL_ORDER.iter() {
            let Some(x)=layout.x[slot] else {continue;};
            let width=layout.widths[slot];
            let rect=card.rect(cx+x-width/2.0-3.0,1.0,width+6.0,28.0);
            let active=self.resident_status_data_available(slot,self.monitor_last.as_ref(),now);
            let c=card.faded(if active{1.0}else{0.38}).ghost();
            let text=self.resident_status_text(slot,self.monitor_last.as_ref(),now,true);
            let size=if matches!(slot,CPU_INFO|GPU_INFO|MEMORY_INFO|DISK_INFO|TEMPERATURE){11.0}else{12.0};
            c.text(cx+x,21.0,&text,size,W::Bold,theme::WHITE,H::Center);
            ui.interact(rect,ui.id().with(("resident-status",slot)),egui::Sense::hover()).on_hover_text(tail_label(slot));
        }
        if let Some(palette)=self.skin_palette() {
            let end=crate::skin_paint::shape_count(ui.ctx(),ui.layer_id());
            crate::skin_paint::recolor_layer(ui.ctx(),ui.layer_id(),&[(content_from,end)],&palette);
        }
        if let Some((slot,index))=movement{
            if let Some(from)=order.iter().position(|value|*value==slot){order.remove(from);order.insert(index,slot);self.settings.resident_order=order;}
        }
        ui.painter().text(Pos2::new(stage.center().x,stage.bottom()-22.0),egui::Align2::CENTER_CENTER,"按住图标左右拖动",theme::font(11.0,W::Regular),Color32::from_gray(160));
        ui.add_space(8.0);
        ui.label(RichText::new("灰色图标为暂未显示的组件；位置会保留。网速及之后的组件按下方列表顺序排在末尾。").size(11.0).color(ui.visuals().weak_text_color()));
    }
}
