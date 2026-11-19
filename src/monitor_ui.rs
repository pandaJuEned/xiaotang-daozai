//! 系统监控的设置层：设置窗口里的「系统监控」与「流量历史」两个页签。
//!
//! 仪表盘窗口本体在 `monitor_widget.rs`。这一层不读系统、不落盘业务数据：
//! 数值一律来自 `Env.monitor`（采集层推送），配置改动走 `monitor.json`
//! 与 `Cmd::MonitorReconfigure`，让外壳下一帧重建采集服务。

use eframe::egui::{self, Color32, RichText};

use crate::app::{App, Cmd};
use crate::extras::{setting_number, setting_row, settings_card, settings_label, settings_segmented};
use crate::monitor::SpeedUnit;
use crate::paint::{self, Card, H};
use crate::theme::{self, W};

/// 实况卡里的紧凑速率：`1.2M` / `300K`。
fn brief(bps: u64) -> String {
    let v = bps as f32;
    if v >= 1_048_576.0 {
        format!("{:.1}M", v / 1_048_576.0)
    } else if v >= 1024.0 {
        format!("{:.0}K", v / 1024.0)
    } else {
        format!("{v:.0}")
    }
}

fn pct(v: Option<f32>) -> String {
    v.map(|x| format!("{x:.0}%")).unwrap_or_else(|| "--".into())
}

/// 24 单位网格的小显示器图标（实况卡左上角的「本机」标记）。
fn pc_glyph(c: &Card, cx: f32, cy: f32, size: f32, color: Color32) {
    let s = size / 24.0;
    let px = |v: f32| cx + (v - 12.0) * s;
    let py = |v: f32| cy + (v - 12.0) * s;
    let w = 1.7 * s;
    let rect = egui::Rect::from_min_max(c.pos(px(2.5), py(4.0)), c.pos(px(21.5), py(17.0)));
    c.painter
        .rect(rect, 2.0 * s, Color32::TRANSPARENT, egui::Stroke::new(w, color), egui::StrokeKind::Inside);
    c.painter
        .line_segment([c.pos(px(8.5), py(20.5)), c.pos(px(15.5), py(20.5))], egui::Stroke::new(w, color));
    c.painter
        .line_segment([c.pos(px(12.0), py(17.0)), c.pos(px(12.0), py(20.5))], egui::Stroke::new(w, color));
}

impl App {
    /// 设置页顶部的实况卡：照天气实况卡的结构 —— 左上机器名、左侧大数字、底部一行指标、
    /// 右下更新时间与刷新按钮。数值取最近一次采集快照，比另开一条通道省事。
    fn monitor_settings_hero(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let width = ui.available_width();
        let height = 150.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let c = Card::new(ui, rect.min, rect);
        let (top, bottom) = (Color32::from_rgb(38, 56, 86), Color32::from_rgb(21, 29, 43));
        c.vgrad(0.0, 0.0, width, height, 14.0, top, bottom);
        c.glow(width - 74.0, 66.0, 64.0, Color32::from_rgba_unmultiplied(61, 158, 255, 70));
        crate::monitor_page::cpu_glyph(&c, width - 74.0, 66.0, 76.0, Color32::from_rgba_unmultiplied(150, 200, 255, 210));

        let soft = Color32::from_white_alpha(205);
        let faint = Color32::from_white_alpha(160);

        let Some(m) = self.monitor_last else {
            c.text(22.0, 40.0, "系统监控", 15.0, W::Semibold, theme::WHITE, H::Left);
            c.text(22.0, 84.0, "还没有监控数据", 22.0, W::Bold, theme::WHITE, H::Left);
            c.text(22.0, 108.0, "打开下面的「启用系统监控」后，这里会显示实时读数。", 11.5, W::Regular, soft, H::Left);
            return;
        };

        // 左上：本机 + 计算机名
        pc_glyph(&c, 26.0, 30.0, 15.0, theme::WHITE);
        let name_w = c.text(35.0, 35.0, "本机", 15.0, W::Semibold, theme::WHITE, H::Left);
        let machine = crate::monitor::machine_name();
        c.text(
            35.0 + name_w + 8.0,
            35.0,
            &c.fit(&machine, 11.5, W::Regular, (width - 220.0 - name_w).max(40.0)),
            11.5,
            W::Regular,
            faint,
            H::Left,
        );

        // 左侧主指标：CPU 占用
        let cpu = m.cpu_percent.map(|v| format!("{v:.0}")).unwrap_or_else(|| "--".into());
        let nw = c.text(22.0, 88.0, &cpu, 46.0, W::Bold, theme::WHITE, H::Left);
        c.text(22.0 + nw + 2.0, 88.0, "%", 20.0, W::Bold, Color32::from_white_alpha(190), H::Left);
        let label_x = 28.0 + nw + 22.0;
        let state = match m.cpu_percent {
            Some(v) if v < 30.0 => "负载较低",
            Some(v) if v < 65.0 => "负载正常",
            Some(_) => "负载较高",
            None => "等待采样",
        };
        c.text(label_x, 70.0, state, 14.0, W::Semibold, theme::WHITE, H::Left);
        let freq = m.cpu_mhz.map(|v| format!("{:.2} GHz", v as f32 / 1000.0)).unwrap_or_else(|| "频率 --".into());
        // 没有 ACPI 热区时退回显卡温度，并标上 GPU，避免看起来像机身温度。
        let temperature = m
            .temperature_c
            .map(|v| format!("{v:.0}°C"))
            .or_else(|| m.gpu.temp_c.map(|v| format!("GPU {v:.0}°C")))
            .unwrap_or_else(|| "温度 --".into());
        c.text(label_x, 89.0, &format!("{freq} · {temperature}"), 11.5, W::Regular, soft, H::Left);

        // 底部一行：网速 / 内存 / GPU / 磁盘，半透明底衬上以细线分隔。
        let mut x = 24.0;
        let stats = [
            format!("↓{}  ↑{}", brief(m.down_bps), brief(m.up_bps)),
            format!("内存 {}", pct(m.mem_percent())),
            format!("GPU {}", pct(m.gpu_percent)),
            format!("磁盘 {}", pct(m.disk_percent)),
        ];
        let limit = width - 172.0;
        c.fill(12.0, 108.0, width - 24.0, 32.0, 10.0, Color32::from_black_alpha(46));
        for text in stats {
            let w = c.text_w(&text, 11.0, W::Regular);
            if x + w > limit {
                break;
            }
            if x > 24.0 {
                c.fill(x - 8.0, 117.0, 1.0, 14.0, 0.5, Color32::from_white_alpha(50));
            }
            c.text(x, 128.5, &text, 11.0, W::Regular, soft, H::Left);
            x += w + 14.0;
        }

        // 右下：更新时间 + 圆形按钮（重启采集服务，不动曲线）。
        let (bx, by) = (width - 32.0, 124.0);
        let button = ui
            .interact(c.rect(bx - 13.0, by - 13.0, 26.0, 26.0), ui.id().with("monitor-hero-refresh"), egui::Sense::click())
            .on_hover_text("重启采集服务");
        let hover = ctx.animate_bool_with_time(button.id, button.hovered(), 0.12);
        c.circle(bx, by, 13.0, Color32::from_white_alpha((34.0 + 40.0 * hover) as u8));
        paint::reset(&c.scaled(bx, by, 0.95), bx, by, 0.0, theme::WHITE);
        let stamp = self
            .monitor_trend
            .back()
            .map(|s| {
                use chrono::TimeZone;
                chrono::Local
                    .timestamp_opt(s.at, 0)
                    .single()
                    .map(|t| t.format("%H:%M").to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        if !stamp.is_empty() {
            c.text(bx - 20.0, 128.5, &format!("更新于 {stamp}"), 11.0, W::Regular, faint, H::Right);
        }
        if button.clicked() {
            self.pending.push(Cmd::MonitorReconfigure);
        }
    }

    /// 设置窗口 tab 13：采集服务、显示、单位与阈值。
    pub(crate) fn monitor_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        self.monitor_settings_hero(ui);
        // monitor.json 不在 `settings_ui` 的 `Settings` diff 里，这里单独做局部 diff。
        let before = format!("{:?}", self.monitor_cfg);
        settings_label(ui, "采集服务");
        settings_card(ui, |ui| {
            setting_row(ui, "启用系统监控", "后台按采样间隔读取网速、CPU 与内存", &mut self.monitor_cfg.enabled);
            ui.separator();
            setting_number(ui, "采样间隔", &mut self.monitor_cfg.interval_ms, 250u64, 60000u64, " ms", 250.0);
            ui.add_space(3.0);
            ui.small("250–60000 毫秒；修改任意配置后采集服务会在下一帧重启。");
        });
        settings_label(ui, "网卡");
        settings_card(ui, |ui| {
            let selected = self.monitor_cfg.interface_index;
            let label = if selected == 0 {
                "全部网卡".to_string()
            } else {
                self.monitor_interfaces
                    .iter()
                    .find(|(i, _)| *i == selected)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| format!("网卡 {selected}（已断开）"))
            };
            egui::ComboBox::from_id_salt("monitor-interface")
                .selected_text(label)
                .width(ui.available_width().min(300.0))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.monitor_cfg.interface_index, 0, "全部网卡");
                    for (index, name) in self.monitor_interfaces.clone() {
                        ui.selectable_value(&mut self.monitor_cfg.interface_index, index, name);
                    }
                });
            ui.add_space(3.0);
            ui.small("选择单块网卡后只统计它；切换会重启采集服务并重置速率基准。");
        });
        settings_label(ui, "显示");
        settings_card(ui, |ui| {
            setting_row(ui, "常驻栏显示网速", "收起时在岛上显示实时下行/上行速率", &mut self.settings.show_net_speed);
            ui.separator();
            setting_row(ui, "显示监控窗口", "独立仪表盘；也可从托盘菜单开关", &mut self.monitor_open);
            ui.separator();
            setting_row(ui, "查看其他设备的系统监控", "只接收 CPU / 内存 / 硬盘 / 温度 / 显卡这类数字，不传进程名；默认关闭", &mut self.settings.show_remote_monitor);
            ui.add_space(3.0);
            ui.small("窗口置顶、鼠标穿透、贴靠右下角在仪表盘右上角齿轮里切换。");
        });
        if self.settings.show_remote_monitor {
            settings_label(ui, "其他设备");
            settings_card(ui, |ui| {
                let remote: Vec<_> = self
                    .sync
                    .devices
                    .iter()
                    .filter(|device| device.online && device.id != self.sync_device.id)
                    .filter_map(|device| device.monitor().map(|monitor| (device.name.clone(), monitor)))
                    .collect();
                if remote.is_empty() {
                    ui.small("暂时没有其他在线设备上报监控；两台都启用同步并保持在线后，这里会出现读数。");
                    return;
                }
                for (index, (name, monitor)) in remote.iter().enumerate() {
                    if index > 0 {
                        ui.separator();
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(name).size(13.0));
                    });
                    let value = |value: Option<f32>, suffix: &str| {
                        value
                            .map(|value| format!("{value:.0}{suffix}"))
                            .unwrap_or_else(|| "--".into())
                    };
                    ui.horizontal_wrapped(|ui| {
                        for (label, text) in [
                            ("CPU", value(monitor.cpu, "%")),
                            ("内存", value(monitor.mem, "%")),
                            ("硬盘", value(monitor.disk, "%")),
                            ("温度", value(monitor.temp, "°")),
                            ("显卡", value(monitor.gpu, "%")),
                        ] {
                            ui.label(RichText::new(format!("{label} {text}")).size(11.5).color(ui.visuals().weak_text_color()));
                            ui.add_space(8.0);
                        }
                    });
                }
                ui.add_space(5.0);
                if ui.button("打开异地监控窗口").clicked() {
                    self.remote_monitor_open = true;
                }
            });
        }
        settings_label(ui, "单位");
        settings_card(ui, |ui| {
            setting_row(ui, "以字节为单位", "关闭后按比特(bit)显示", &mut self.monitor_cfg.display.unit_byte);
            ui.separator();
            setting_row(ui, "简洁模式", "减少小数位并省略单位中的 B", &mut self.monitor_cfg.display.short_mode);
            ui.separator();
            setting_row(ui, "隐藏单位", "只显示数值", &mut self.monitor_cfg.display.hide_unit);
            ui.separator();
            setting_row(ui, "数值与单位间留空格", "", &mut self.monitor_cfg.display.space);
        });
        settings_card(ui, |ui| {
            ui.label(RichText::new("速率档位").size(13.0));
            settings_segmented(
                ui,
                &mut self.monitor_cfg.display.speed_unit,
                &[(SpeedUnit::Auto, "自动"), (SpeedUnit::Kilo, "K"), (SpeedUnit::Mega, "M")],
                64.0,
            );
        });
        settings_label(ui, "阈值提醒");
        settings_card(ui, |ui| {
            setting_number(ui, "今日流量", &mut self.monitor_cfg.thresholds.today_traffic_bytes, 0u64, u64::MAX / 2, " B", 1048576.0);
            ui.separator();
            setting_number(ui, "CPU 占用", &mut self.monitor_cfg.thresholds.cpu_percent, 0u8, 100u8, " %", 1.0);
            ui.separator();
            setting_number(ui, "内存占用", &mut self.monitor_cfg.thresholds.mem_percent, 0u8, 100u8, " %", 1.0);
            ui.separator();
            setting_number(ui, "提醒冷却", &mut self.monitor_cfg.thresholds.cooldown_secs, 0u64, 3600u64, " 秒", 30.0);
            ui.add_space(3.0);
            ui.small("阈值设为 0 表示关闭。越限时弹出托盘气泡；同一条提醒在冷却时间内不重复。");
        });
        if before != format!("{:?}", self.monitor_cfg) {
            let _ = self.monitor_cfg.save();
            self.pending.push(Cmd::MonitorReconfigure);
        }
    }

    /// 设置窗口 tab 14：历史流量（日/周/月汇总 + 最近 14 天柱状图 + 每日明细）。
    pub(crate) fn monitor_history_settings(&mut self, ui: &mut egui::Ui) {
        use crate::monitor::{format_size, HistoryStore};
        if self.monitor_history.is_none() {
            self.monitor_history = Some(HistoryStore::load());
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("按日归档的本机流量，最多保留 400 天。").size(12.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    self.monitor_history = Some(HistoryStore::load());
                }
            });
        });
        let Some(store) = self.monitor_history.clone() else { return; };
        let d = self.monitor_cfg.display;
        let today = store.totals_since(1);
        let week = store.totals_since(7);
        let month = store.totals_since(30);
        settings_label(ui, "汇总");
        settings_card(ui, |ui| {
            for (name, v) in [("今日", today), ("近 7 天", week), ("近 30 天", month)] {
                ui.label(format!(
                    "{name}   ↓{}   ↑{}   合计 {}",
                    format_size(v.down_bytes, &d),
                    format_size(v.up_bytes, &d),
                    format_size(v.down_bytes.saturating_add(v.up_bytes), &d)
                ));
            }
        });
        settings_label(ui, "最近 14 天");
        settings_card(ui, |ui| {
            let days: Vec<_> = store.days().iter().rev().take(14).rev().collect();
            if days.is_empty() {
                ui.label("暂无历史数据");
                return;
            }
            let max = days.iter().map(|day| day.total()).max().unwrap_or(1).max(1);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 96.0), egui::Sense::hover());
            let painter = ui.painter();
            let slot = rect.width() / days.len() as f32;
            let bar_w = (slot * 0.6).min(28.0);
            for (i, day) in days.iter().enumerate() {
                let h = (day.total() as f32 / max as f32) * (rect.height() - 18.0).max(1.0);
                let x = rect.left() + slot * (i as f32 + 0.5);
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(x - bar_w / 2.0, rect.bottom() - 16.0 - h),
                        egui::vec2(bar_w, h),
                    ),
                    2.0,
                    Color32::from_rgb(90, 160, 220),
                );
                painter.text(
                    egui::pos2(x, rect.bottom() - 4.0),
                    egui::Align2::CENTER_CENTER,
                    day.date.get(5..).unwrap_or(day.date.as_str()),
                    egui::FontId::proportional(9.0),
                    Color32::GRAY,
                );
            }
        });
        settings_label(ui, "每日明细（最近 30 天）");
        settings_card(ui, |ui| {
            egui::Grid::new("monitor-history-grid").striped(true).show(ui, |ui| {
                ui.label(RichText::new("日期").strong());
                ui.label(RichText::new("下行").strong());
                ui.label(RichText::new("上行").strong());
                ui.label(RichText::new("合计").strong());
                ui.end_row();
                for day in store.days().iter().rev().take(30) {
                    ui.label(&day.date);
                    ui.label(format_size(day.down_bytes, &d));
                    ui.label(format_size(day.up_bytes, &d));
                    ui.label(format_size(day.total(), &d));
                    ui.end_row();
                }
            });
        });
    }
}
