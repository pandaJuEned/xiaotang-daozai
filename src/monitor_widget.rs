//! 系统监控仪表盘（独立窗口）。
//!
//! 版式对照参考原型 `system-monitor-ui/index.html`：780×452 的深色圆角卡片，
//! 顶部「系统监控 / 时钟 / 刷新 / 齿轮」，中部 CPU 主指标 + 四张指标卡，
//! 底部六列概览 + 双折线 + 上一页/下一页。
//!
//! 三个窗口级行为都走 egui 视口命令，不碰 Win32 窗口层次：
//! - 拖动：`ViewportCommand::StartDrag`（winit 的 `drag_window`）
//! - 置顶：`ViewportBuilder::with_always_on_top`
//! - 鼠标穿透：`ViewportCommand::MousePassthrough`（winit 的 `set_cursor_hittest`）
//!
//! 所有绘制坐标以设计尺寸为准，再按 `k` 等比缩放，窗口拉大拉小都不会错位。

use chrono::TimeZone;
use eframe::egui::{self, Align2, Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::app::{App, Env};
use crate::monitor::{format_speed, DisplayConfig, MonitorSnapshot, TrendSample};
use crate::theme::{self, W};

/// 设计画布尺寸（与原型一致）。
const DW: f32 = 780.0;
const DH: f32 = 452.0;
/// 底部概览列数。
const COLS: usize = 6;

const BLUE: Color32 = Color32::from_rgb(61, 158, 255);
const YELLOW: Color32 = Color32::from_rgb(255, 176, 46);
const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
const PURPLE: Color32 = Color32::from_rgb(191, 90, 242);
const CYAN: Color32 = Color32::from_rgb(100, 210, 255);
const HOT: Color32 = Color32::from_rgb(255, 93, 93);
const INK: Color32 = Color32::from_rgb(238, 241, 245);
const SUB: Color32 = Color32::from_rgb(154, 160, 168);
const SUB2: Color32 = Color32::from_rgb(134, 140, 149);
const FAINT: Color32 = Color32::from_rgb(93, 99, 108);

/* ---------------- 画布：设计坐标 → 屏幕坐标 ---------------- */

struct Canvas {
    paint: egui::Painter,
    k: f32,
    o: Pos2,
}

impl Canvas {
    fn at(&self, x: f32, y: f32) -> Pos2 {
        self.o + Vec2::new(x * self.k, y * self.k)
    }

    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.at(x, y), Vec2::new(w * self.k, h * self.k))
    }

    fn text(&self, x: f32, y: f32, text: &str, size: f32, w: W, color: Color32, align: Align2) {
        self.paint.text(self.at(x, y), align, text, theme::font(size * self.k, w), color);
    }

    /// 文本宽度（设计单位），用于右对齐与紧跟其后的排版。
    fn text_w(&self, text: &str, size: f32, w: W) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        self.paint
            .layout_no_wrap(text.to_owned(), theme::font(size * self.k, w), Color32::WHITE)
            .size()
            .x
            / self.k
    }

    fn seg(&self, a: Pos2, b: Pos2, width: f32, color: Color32) {
        self.paint.line_segment([a, b], Stroke::new(width * self.k, color));
    }

    fn round(&self, x: f32, y: f32, w: f32, h: f32, r: f32, fill: Color32) {
        self.paint.rect(self.rect(x, y, w, h), r * self.k, fill, Stroke::NONE, StrokeKind::Inside);
    }

    fn outline(&self, x: f32, y: f32, w: f32, h: f32, r: f32, stroke: Stroke) {
        self.paint.rect(
            self.rect(x, y, w, h),
            r * self.k,
            Color32::TRANSPARENT,
            Stroke::new(stroke.width * self.k, stroke.color),
            StrokeKind::Inside,
        );
    }

    fn dot(&self, x: f32, y: f32, r: f32, color: Color32) {
        self.paint.circle_filled(self.at(x, y), r * self.k, color);
    }

    /// 柔光：叠几层递减透明度，避免圆边生硬。
    fn glow(&self, x: f32, y: f32, r: f32, color: Color32) {
        for i in 0..5 {
            let t = 1.0 - i as f32 / 5.0;
            let a = (color.a() as f32 * 0.16 * t) as u8;
            self.dot(x, y, r * (1.0 - i as f32 * 0.13), Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a));
        }
    }
}

/* ---------------- 图标（24 单位网格手绘） ---------------- */

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Cpu,
    Mem,
    Gpu,
    Hdd,
    Net,
    Pc,
    Gear,
}

fn glyph(c: &Canvas, kind: Glyph, cx: f32, cy: f32, size: f32, color: Color32) {
    let s = size / 24.0;
    let px = |v: f32| cx + (v - 12.0) * s;
    let py = |v: f32| cy + (v - 12.0) * s;
    let w = 1.8 * s;
    let line = |x1: f32, y1: f32, x2: f32, y2: f32| {
        c.seg(c.at(px(x1), py(y1)), c.at(px(x2), py(y2)), w, color)
    };
    let ring = |x: f32, y: f32, ww: f32, hh: f32, r: f32| {
        let rect = Rect::from_min_max(c.at(px(x), py(y)), c.at(px(x + ww), py(y + hh)));
        c.paint.rect(rect, r * s * c.k, Color32::TRANSPARENT, Stroke::new(w, color), StrokeKind::Inside);
    };
    match kind {
        Glyph::Cpu => {
            ring(5.0, 5.0, 14.0, 14.0, 2.5);
            ring(9.5, 9.5, 5.0, 5.0, 1.0);
            for v in [9.0, 15.0] {
                line(v, 2.5, v, 5.0);
                line(v, 19.0, v, 21.5);
                line(2.5, v, 5.0, v);
                line(19.0, v, 21.5, v);
            }
        }
        Glyph::Mem => {
            ring(3.0, 5.0, 18.0, 9.0, 2.0);
            for v in [6.0, 10.0, 14.0, 18.0] {
                line(v, 14.0, v, 17.5);
            }
            for v in [8.5, 12.0, 15.5] {
                line(v, 10.5, v, 8.5);
            }
        }
        Glyph::Gpu => {
            ring(2.5, 7.0, 16.0, 8.5, 2.0);
            line(18.5, 9.5, 21.5, 9.5);
            line(21.5, 9.5, 21.5, 13.0);
            line(21.5, 13.0, 18.5, 13.0);
            line(6.0, 15.5, 6.0, 18.5);
            line(14.0, 15.5, 14.0, 18.5);
            c.paint
                .circle_stroke(c.at(px(9.0), py(11.2)), 2.4 * s * c.k, Stroke::new(w, color));
        }
        Glyph::Hdd => {
            ring(2.6, 6.4, 18.8, 11.2, 2.0);
            line(2.6, 12.0, 21.4, 12.0);
            c.dot(px(6.5), py(15.4), 1.1 * s, color);
            c.dot(px(10.0), py(15.4), 1.1 * s, color);
        }
        Glyph::Net => {
            line(8.0, 3.5, 8.0, 13.0);
            line(4.8, 10.0, 8.0, 13.2);
            line(11.2, 10.0, 8.0, 13.2);
            line(16.0, 20.5, 16.0, 11.0);
            line(12.8, 14.0, 16.0, 10.8);
            line(19.2, 14.0, 16.0, 10.8);
        }
        Glyph::Pc => {
            ring(2.5, 4.0, 19.0, 13.0, 2.0);
            line(8.5, 20.5, 15.5, 20.5);
            line(12.0, 17.0, 12.0, 20.5);
        }
        Glyph::Gear => {
            let center = c.at(px(12.0), py(12.0));
            c.paint.circle_stroke(center, 3.0 * s * c.k, Stroke::new(w, color));
            c.paint.circle_stroke(center, 6.6 * s * c.k, Stroke::new(w * 0.85, color));
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (sn, cs) = a.sin_cos();
                line(12.0 + cs * 7.4, 12.0 + sn * 7.4, 12.0 + cs * 9.6, 12.0 + sn * 9.6);
            }
        }
    }
}

/* ---------------- 数值与配色 ---------------- */

fn load_color(v: f32) -> Color32 {
    if v < 40.0 {
        BLUE
    } else if v < 75.0 {
        YELLOW
    } else {
        HOT
    }
}

fn net_color(bytes_per_sec: f32) -> Color32 {
    if bytes_per_sec < 1_048_576.0 {
        BLUE
    } else if bytes_per_sec < 4_194_304.0 {
        YELLOW
    } else {
        HOT
    }
}

fn pct(v: Option<f32>) -> String {
    v.map(|x| format!("{x:.0}%")).unwrap_or_else(|| "--".into())
}

fn load_state(cpu: Option<f32>) -> &'static str {
    match cpu {
        Some(v) if v < 30.0 => "负载较低",
        Some(v) if v < 65.0 => "负载正常",
        Some(_) => "负载较高",
        None => "等待采样",
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_073_741_824.0)
}

fn clock_text() -> String {
    let now = chrono::Local::now();
    let idx = now.format("%w").to_string().parse::<usize>().unwrap_or(0) % 7;
    let week = "日一二三四五六".chars().nth(idx).unwrap_or('?');
    format!("{}月{}日 周{} {}", now.format("%-m"), now.format("%-d"), week, now.format("%H:%M"))
}

fn stamp(at: i64) -> String {
    chrono::Local
        .timestamp_opt(at, 0)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

/* ---------------- 主绘制 ---------------- */

impl App {
    /// 独立仪表盘窗口。关闭走 `close_requested`；穿透由托盘菜单或齿轮菜单关闭。
    pub fn monitor_window(&mut self, ctx: &egui::Context, env: &Env) {
        if !self.monitor_open {
            return;
        }
        let id = egui::ViewportId::from_hash_of("daozai-monitor");
        let mut builder = egui::ViewportBuilder::default()
            .with_title("系统监控")
            .with_inner_size([DW, DH])
            .with_min_inner_size([520.0, 300.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_taskbar(false);
        if self.monitor_pinned {
            builder = builder.with_always_on_top();
        }

        let snap = env.monitor;
        let display = self.monitor_cfg.display;
        // 六列：样本不足时靠右对齐，最新一列永远在最右。
        let picked = self.monitor_trend.pick(COLS);
        let mut cols: [Option<TrendSample>; COLS] = [None; COLS];
        let start = COLS.saturating_sub(picked.len());
        for (i, s) in picked.iter().enumerate() {
            cols[start + i] = Some(*s);
        }

        ctx.show_viewport_immediate(id, builder, |ctx, _| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.monitor_open = false;
            }
            // 首次打开居中；之后位置完全交给拖动与「贴靠右下角」，不再干预。
            let placed = egui::Id::new("monitor-placed");
            if !ctx.data(|d| d.get_temp::<bool>(placed).unwrap_or(false)) {
                ctx.data_mut(|d| d.insert_temp(placed, true));
                let screen = ctx.screen_rect();
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    screen.center().x - DW * 0.5,
                    screen.center().y - DH * 0.5,
                )));
            }
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                self.dashboard(ui, ctx, snap, &display, &cols);
            });
            // 穿透状态只在变化时下发；必须在这里发，命令才会落到仪表盘视口
            // 而不是主窗口（发错会让岛体变成点不动）。
            let key = egui::Id::new("monitor-passthrough");
            let applied = ctx.data(|d| d.get_temp::<bool>(key).unwrap_or(false));
            if applied != self.monitor_click {
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(self.monitor_click));
                ctx.data_mut(|d| d.insert_temp(key, self.monitor_click));
            }
            // 时钟自己保活：即使采集被关掉，秒也要走。
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        });
    }

    /// 异地监控：只读其他在线设备通过 presence 带来的纯数字快照。
    pub fn remote_monitor_window(&mut self, ctx: &egui::Context) {
        if !self.remote_monitor_open {
            return;
        }
        let id = egui::ViewportId::from_hash_of("daozai-remote-monitor");
        let builder = egui::ViewportBuilder::default()
            .with_title("异地监控")
            .with_inner_size([520.0, 380.0])
            .with_min_inner_size([420.0, 280.0]);
        ctx.show_viewport_immediate(id, builder, |ctx, _| {
            if ctx.input(|input| input.viewport().close_requested()) {
                self.remote_monitor_open = false;
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("其他设备");
                ui.label(
                    egui::RichText::new("只有在线、启用同步并上报了监控的设备会出现；数值每 30 秒刷新一次。")
                        .size(11.5)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.separator();
                let remote: Vec<_> = self
                    .sync
                    .devices
                    .iter()
                    .filter(|device| device.online && device.id != self.sync_device.id)
                    .filter_map(|device| device.monitor().map(|monitor| (device.name.clone(), monitor)))
                    .collect();
                if remote.is_empty() {
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("暂时没有其他在线设备").size(14.0));
                        ui.label(
                            egui::RichText::new("两台都启用同步并保持在线后，这里会显示 CPU、内存、温度等读数。")
                                .size(11.5)
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (name, monitor) in remote {
                        egui::Frame::new()
                            .fill(ui.visuals().faint_bg_color)
                            .corner_radius(10.0)
                            .inner_margin(egui::Margin::symmetric(14, 10))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(&name).size(14.0).strong());
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new("● 在线")
                                                    .size(11.0)
                                                    .color(egui::Color32::from_rgb(90, 190, 120)),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(4.0);
                                ui.horizontal_wrapped(|ui| {
                                    let metric = |ui: &mut egui::Ui, label: &str, value: Option<f32>, suffix: &str| {
                                        ui.label(
                                            egui::RichText::new(label)
                                                .size(11.0)
                                                .color(ui.visuals().weak_text_color()),
                                        );
                                        ui.label(
                                            egui::RichText::new(
                                                value
                                                    .map(|value| format!("{value:.0}{suffix}"))
                                                    .unwrap_or_else(|| "--".into()),
                                            )
                                            .size(13.0)
                                            .strong(),
                                        );
                                        ui.add_space(10.0);
                                    };
                                    metric(ui, "CPU", monitor.cpu, "%");
                                    metric(ui, "内存", monitor.mem, "%");
                                    metric(ui, "硬盘", monitor.disk, "%");
                                    metric(ui, "温度", monitor.temp, "°");
                                    metric(ui, "显卡", monitor.gpu, "%");
                                });
                            });
                        ui.add_space(8.0);
                    }
                });
            });
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        });
    }

    fn dashboard(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        snap: Option<MonitorSnapshot>,
        d: &DisplayConfig,
        cols: &[Option<TrendSample>; COLS],
    ) {
        let full = ui.max_rect();
        if full.width() < 120.0 || full.height() < 80.0 {
            return;
        }
        let k = (full.width() / DW).min(full.height() / DH).clamp(0.35, 1.6);
        let card = Rect::from_center_size(full.center(), Vec2::new(DW * k, DH * k));
        let c = Canvas { paint: ui.painter().clone(), k, o: card.min };

        self.paint_shell(&c, ui, ctx);
        let has = snap.is_some();
        let m = snap.unwrap_or_default();
        self.paint_header(&c, ui);
        self.paint_hero(&c, &m, has);

        // 底部翻页按钮压在左右边缘；齿轮菜单展开时让位，避免和弹出层抢点击。
        let prev = c.rect(40.0, 140.0, 24.0, 40.0);
        let next = c.rect(716.0, 140.0, 24.0, 40.0);
        let page = self.monitor_page;
        if !self.monitor_menu {
            for (rect, dir) in [(prev, -1.0f32), (next, 1.0f32)] {
                let res = ui.interact(rect, ui.id().with(("monitor-nav", dir as i32)), Sense::click());
                if res.hovered() {
                    c.round(
                        (rect.left() - c.o.x) / c.k,
                        (rect.top() - c.o.y) / c.k,
                        24.0,
                        40.0,
                        8.0,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 14),
                    );
                }
                let cx = (rect.center().x - c.o.x) / c.k;
                let cy = (rect.center().y - c.o.y) / c.k;
                let color = if res.hovered() { Color32::from_rgb(201, 206, 214) } else { FAINT };
                for step in [-1.0f32, 1.0] {
                    let dx = dir * 2.4 * step;
                    c.seg(c.at(cx + dx, cy - 5.0), c.at(cx + dx + dir * 4.4, cy), 1.8, color);
                    c.seg(c.at(cx + dx + dir * 4.4, cy), c.at(cx + dx, cy + 5.0), 1.8, color);
                }
                if res.clicked() {
                    self.monitor_page = (page + 1) % 2;
                }
            }
        }

        self.paint_cards(&c, &m, d, has);
        self.paint_bottom(&c, ui, cols, d, page);
        // 弹出层最后画、最后注册交互，才能压在卡片之上。
        if self.monitor_menu {
            self.paint_menu(&c, ui, ctx);
        }
    }

    /// 背景卡片、投影与整窗拖动。
    fn paint_shell(&mut self, c: &Canvas, ui: &mut egui::Ui, ctx: &egui::Context) {
        for i in 0..3 {
            let grow = (i + 1) as f32 * 7.0;
            c.round(
                -grow,
                -grow,
                DW + grow * 2.0,
                DH + grow * 2.0,
                22.0 + grow,
                Color32::from_black_alpha(10),
            );
        }
        c.round(0.0, 0.0, DW, DH, 22.0, Color32::from_rgb(12, 14, 18));
        c.round(0.0, 0.0, DW, DH * 0.55, 22.0, Color32::from_rgba_unmultiplied(24, 26, 33, 90));
        c.outline(0.0, 0.0, DW, DH, 22.0, Stroke::new(1.0_f32, Color32::from_white_alpha(20)));

        // 整块可拖：背景先注册，交互控件后注册即可压在它上面。
        let drag = ui.interact(c.rect(0.0, 0.0, DW, DH), ui.id().with("monitor-drag"), Sense::drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    fn paint_header(&mut self, c: &Canvas, ui: &mut egui::Ui) {
        c.text(22.0, 37.0, "系统", 23.0, W::Bold, INK, Align2::LEFT_CENTER);
        let w1 = c.text_w("系统", 23.0, W::Bold);
        c.text(22.0 + w1 + 2.0, 37.0, "监控", 23.0, W::Bold, BLUE, Align2::LEFT_CENTER);

        // 右侧从右往左排：齿轮(728..758) → 刷新 → 时钟
        let gear = c.rect(728.0, 16.0, 30.0, 30.0);
        let gear_res = ui.interact(gear, ui.id().with("monitor-gear"), Sense::click());
        glyph(
            c,
            Glyph::Gear,
            743.0,
            31.0,
            18.0,
            if gear_res.hovered() || self.monitor_menu {
                Color32::WHITE
            } else {
                Color32::from_rgb(198, 203, 210)
            },
        );
        if gear_res.clicked() {
            self.monitor_menu = !self.monitor_menu;
        }

        let refresh_w = c.text_w("刷新", 14.0, W::Regular).max(28.0);
        let refresh_x = 728.0 - 14.0 - refresh_w;
        let refresh = c.rect(refresh_x, 16.0, refresh_w, 30.0);
        let refresh_res = ui.interact(refresh, ui.id().with("monitor-refresh"), Sense::click());
        c.text(
            refresh_x + refresh_w * 0.5,
            31.0,
            "刷新",
            14.0,
            W::Regular,
            if refresh_res.hovered() { Color32::WHITE } else { Color32::from_rgb(215, 219, 225) },
            Align2::CENTER_CENTER,
        );
        if refresh_res.clicked() {
            // 「刷新」= 清空曲线重新开始一个采样窗口，不重启采集服务。
            self.monitor_trend.clear();
        }
        if refresh_res.hovered() {
            refresh_res.on_hover_text("清空曲线，重新开始一次采样窗口");
        }

        let clock = clock_text();
        let clock_w = c.text_w(&clock, 15.0, W::Bold) + 32.0;
        let clock_x = refresh_x - 14.0 - clock_w;
        c.round(clock_x, 16.0, clock_w, 30.0, 999.0, Color32::from_rgba_unmultiplied(255, 255, 255, 24));
        c.text(clock_x + clock_w * 0.5, 31.0, &clock, 15.0, W::Bold, INK, Align2::CENTER_CENTER);
    }

    /// 齿轮弹出层：置顶 / 穿透 / 贴靠 / 关闭。
    fn paint_menu(&mut self, c: &Canvas, ui: &mut egui::Ui, ctx: &egui::Context) {
        let x = DW - 26.0 - 198.0;
        let y = 56.0;
        let rows: [(&str, bool); 2] = [("窗口置顶", self.monitor_pinned), ("鼠标穿透", self.monitor_click)];
        let h = 8.0 + 2.0 * 36.0 + 34.0 + 34.0;
        c.round(x, y, 198.0, h, 14.0, Color32::from_rgb(25, 27, 31));
        c.outline(x, y, 198.0, h, 14.0, Stroke::new(1.0_f32, Color32::from_white_alpha(24)));

        for (i, (label, on)) in rows.iter().enumerate() {
            let ry = y + 8.0 + i as f32 * 36.0;
            c.text(x + 16.0, ry + 14.0, label, 13.0, W::Regular, Color32::from_rgb(215, 219, 225), Align2::LEFT_CENTER);
            let sw_x = x + 198.0 - 16.0 - 34.0;
            let sw = c.rect(sw_x, ry + 4.0, 34.0, 20.0);
            let res = ui.interact(sw, ui.id().with(("monitor-sw", i)), Sense::click());
            c.round(sw_x, ry + 4.0, 34.0, 20.0, 999.0, if *on { BLUE } else { Color32::from_rgb(58, 62, 70) });
            c.dot(if *on { sw_x + 24.0 } else { sw_x + 10.0 }, ry + 14.0, 8.0, Color32::WHITE);
            if res.clicked() {
                // 两个都是会话内开关；命令由 `monitor_window` 的同步块统一下发。
                match i {
                    0 => self.monitor_pinned = !self.monitor_pinned,
                    _ => self.monitor_click = !self.monitor_click,
                }
            }
        }

        // 第三行是一次性动作，不做成常亮开关。
        let dock_y = y + 8.0 + 2.0 * 36.0;
        let dock = c.rect(x + 8.0, dock_y, 182.0, 30.0);
        let dock_res = ui.interact(dock, ui.id().with("monitor-dock"), Sense::click());
        if dock_res.hovered() {
            c.round(x + 8.0, dock_y, 182.0, 30.0, 8.0, Color32::from_rgba_unmultiplied(255, 255, 255, 16));
        }
        c.text(
            x + 16.0,
            dock_y + 15.0,
            "贴靠右下角",
            13.0,
            W::Regular,
            if dock_res.hovered() { INK } else { Color32::from_rgb(215, 219, 225) },
            Align2::LEFT_CENTER,
        );
        c.text(
            x + 198.0 - 20.0,
            dock_y + 15.0,
            "›",
            15.0,
            W::Semibold,
            if dock_res.hovered() { INK } else { FAINT },
            Align2::CENTER_CENTER,
        );
        if dock_res.clicked() {
            // 贴到当前屏幕右下角，留 16 点边距。
            let screen = ctx.screen_rect();
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                screen.right() - DW - 16.0,
                screen.bottom() - DH - 16.0,
            )));
        }

        // 关闭窗口放在底部，避免无边框窗口没有关闭入口。
        let close = c.rect(x + 8.0, y + h - 30.0, 182.0, 24.0);
        let close_res = ui.interact(close, ui.id().with("monitor-close"), Sense::click());
        if close_res.hovered() {
            c.round(x + 8.0, y + h - 30.0, 182.0, 24.0, 8.0, Color32::from_rgba_unmultiplied(255, 255, 255, 16));
        }
        c.text(
            x + 16.0,
            y + h - 18.0,
            "关闭监控窗口",
            12.0,
            W::Regular,
            if close_res.hovered() { INK } else { SUB },
            Align2::LEFT_CENTER,
        );
        if close_res.clicked() {
            self.monitor_open = false;
        }
    }

    fn paint_hero(&self, c: &Canvas, m: &MonitorSnapshot, has: bool) {
        let y0 = 68.0;
        c.glow(84.0, y0 + 56.0, 46.0, BLUE);
        glyph(c, Glyph::Cpu, 84.0, y0 + 56.0, 72.0, BLUE);

        let cpu = if has { pct(m.cpu_percent) } else { "--".into() };
        let num = cpu.trim_end_matches('%');
        c.text(136.0, y0 + 88.0, num, 62.0, W::Bold, INK, Align2::LEFT_BOTTOM);
        let nw = c.text_w(num, 62.0, W::Bold);
        c.text(136.0 + nw + 3.0, y0 + 88.0, "%", 28.0, W::Bold, SUB, Align2::LEFT_BOTTOM);

        let state = load_state(if has { m.cpu_percent } else { None });
        c.text(52.0, y0 + 110.0, state, 17.0, W::Bold, INK, Align2::LEFT_CENTER);
        let sw = c.text_w(state, 17.0, W::Bold);
        c.text(52.0 + sw + 8.0, y0 + 110.0, "·", 17.0, W::Regular, FAINT, Align2::LEFT_CENTER);
        let freq = m
            .cpu_mhz
            .filter(|_| has)
            .map(|v| format!("{:.2} GHz", v as f32 / 1000.0))
            .unwrap_or_else(|| "频率 --".into());
        // 没有 ACPI 热区时退回显卡温度，并标上 GPU，避免看起来像机身温度。
        let temperature = m
            .temperature_c
            .filter(|_| has)
            .map(|v| format!("{v:.0}°C"))
            .or_else(|| m.gpu.temp_c.filter(|_| has).map(|v| format!("GPU {v:.0}°C")))
            .unwrap_or_else(|| "温度 --".into());
        c.text(
            52.0 + sw + 22.0,
            y0 + 110.0,
            &format!("{freq} · {temperature}"),
            15.0,
            W::Semibold,
            Color32::from_rgb(201, 206, 214),
            Align2::LEFT_CENTER,
        );

        glyph(c, Glyph::Pc, 60.0, y0 + 136.0, 15.0, SUB);
        c.text(
            72.0,
            y0 + 136.0,
            &format!("本机 · {}", crate::monitor::machine_name()),
            14.0,
            W::Regular,
            SUB,
            Align2::LEFT_CENTER,
        );
    }

    fn paint_cards(&self, c: &Canvas, m: &MonitorSnapshot, d: &DisplayConfig, has: bool) {
        let y0 = 68.0;
        let (cx, cy) = (322.0, 532.0);
        let (r1, r2) = (y0 + 5.0, y0 + 91.0);

        // 内存
        self.card_shell(c, cx, r1, BLUE, Glyph::Mem, "内存");
        let mem = m.mem_percent().filter(|_| has);
        c.text(cx + 46.0, r1 + 45.0, &pct(mem), 19.0, W::Bold, INK, Align2::LEFT_CENTER);
        let mw = c.text_w(&pct(mem), 19.0, W::Bold);
        c.text(
            cx + 46.0 + mw + 5.0,
            r1 + 46.0,
            &format!("{} / {} GB", gib(m.mem_used_bytes), gib(m.mem_total_bytes)),
            12.0,
            W::Regular,
            SUB2,
            Align2::LEFT_CENTER,
        );

        // GPU
        self.card_shell(c, cy, r1, GREEN, Glyph::Gpu, "GPU");
        c.text(cx + 46.0, r1 + 45.0, &pct(m.gpu_percent.filter(|_| has)), 19.0, W::Bold, INK, Align2::LEFT_CENTER);
        if let Some(brief) = m.gpu.brief().filter(|_| has) {
            c.text(cx + 46.0, r1 + 62.0, &brief, 11.0, W::Regular, SUB2, Align2::LEFT_CENTER);
        }

        // 硬盘
        self.card_shell(c, cx, r2, PURPLE, Glyph::Hdd, "硬盘");
        c.text(cx + 46.0, r2 + 40.0, &pct(m.disk_percent.filter(|_| has)), 19.0, W::Bold, INK, Align2::LEFT_CENTER);
        let io = match (m.disk_read_mbps, m.disk_write_mbps) {
            (Some(r), Some(w)) if has => format!("读 {r:.0} · 写 {w:.0} MB/s"),
            _ => "读写 -- · -- MB/s".to_string(),
        };
        c.text(cx + 46.0, r2 + 60.0, &io, 11.0, W::Regular, SUB2, Align2::LEFT_CENTER);

        // 网速
        self.card_shell(c, cy, r2, CYAN, Glyph::Net, "");
        let (down, up) = if has {
            (format_speed(m.down_bps, d), format_speed(m.up_bps, d))
        } else {
            ("--".into(), "--".into())
        };
        c.text(cy + 46.0, r2 + 30.0, "↓", 13.0, W::Bold, CYAN, Align2::LEFT_CENTER);
        c.text(cy + 62.0, r2 + 30.0, &down, 17.0, W::Bold, INK, Align2::LEFT_CENTER);
        c.text(cy + 46.0, r2 + 54.0, "↑", 13.0, W::Bold, SUB, Align2::LEFT_CENTER);
        c.text(cy + 62.0, r2 + 54.0, &up, 13.0, W::Semibold, Color32::from_rgb(201, 206, 214), Align2::LEFT_CENTER);
    }

    fn card_shell(&self, c: &Canvas, x: f32, y: f32, tint: Color32, kind: Glyph, label: &str) {
        c.round(x, y, 200.0, 76.0, 13.0, Color32::from_rgba_unmultiplied(255, 255, 255, 18));
        c.outline(x, y, 200.0, 76.0, 13.0, Stroke::new(1.0_f32, Color32::from_white_alpha(10)));
        glyph(c, kind, x + 25.0, y + 38.0, 21.0, tint);
        if !label.is_empty() {
            c.text(x + 46.0, y + 22.0, label, 12.0, W::Regular, SUB, Align2::LEFT_CENTER);
        }
    }

    fn paint_bottom(
        &self,
        c: &Canvas,
        ui: &mut egui::Ui,
        cols: &[Option<TrendSample>; COLS],
        d: &DisplayConfig,
        page: usize,
    ) {
        c.round(22.0, 252.0, 736.0, 186.0, 16.0, Color32::from_rgb(18, 20, 24));
        c.outline(22.0, 252.0, 736.0, 186.0, 16.0, Stroke::new(1.0_f32, Color32::from_white_alpha(12)));

        let load = page == 0;
        let items: [(&str, Color32); 2] = if load {
            [("CPU 占用", YELLOW), ("内存占用", BLUE)]
        } else {
            [("下行", YELLOW), ("上行", BLUE)]
        };
        let mut lx = 744.0;
        for (label, color) in items.iter().rev() {
            let tw = c.text_w(label, 11.0, W::Regular);
            lx -= tw;
            c.text(lx, 269.0, label, 11.0, W::Regular, SUB, Align2::LEFT_CENTER);
            lx -= 13.0;
            c.dot(lx, 269.0, 4.0, *color);
            lx -= 14.0;
        }

        let mut hovered: Option<usize> = None;
        for i in 0..COLS {
            let cx = 95.0 + i as f32 * 118.0;
            let slot = c.rect(cx - 54.0, 279.0, 108.0, 102.0);
            let res = ui.interact(slot, ui.id().with(("monitor-col", i)), Sense::hover());
            if res.hovered() {
                hovered = Some(i);
            }
            let sample = cols[i];
            let is_now = i == COLS - 1 && sample.is_some();
            if is_now || res.hovered() {
                c.round(cx - 54.0, 279.0, 108.0, 101.0, 12.0, Color32::from_rgba_unmultiplied(255, 255, 255, 26));
            }
            let time = if is_now {
                "现在".to_string()
            } else {
                sample.map(|s| stamp(s.at)).unwrap_or_else(|| "--:--".into())
            };
            c.text(
                cx,
                288.0,
                &time,
                12.0,
                if is_now { W::Bold } else { W::Regular },
                if is_now { INK } else { SUB },
                Align2::CENTER_CENTER,
            );

            let s = sample.unwrap_or_default();
            let has = sample.is_some();
            if load {
                glyph(c, Glyph::Cpu, cx, 310.0, 19.0, load_color(s.cpu));
                c.text(cx, 329.0, &if has { format!("{:.0}%", s.gpu) } else { "--".into() }, 12.0, W::Semibold, BLUE, Align2::CENTER_CENTER);
                c.text(cx, 351.0, &if has { format!("{:.0}%", s.cpu) } else { "--".into() }, 20.0, W::Bold, INK, Align2::CENTER_CENTER);
                c.text(cx, 372.0, &if has { format!("{:.0}%", s.mem) } else { "--".into() }, 12.0, W::Regular, SUB2, Align2::CENTER_CENTER);
            } else {
                glyph(c, Glyph::Net, cx, 310.0, 19.0, net_color(s.down));
                let down = if has { format_speed(s.down as u64, d) } else { "--".into() };
                let up = if has { format_speed(s.up as u64, d) } else { "--".into() };
                c.text(cx, 351.0, &down, 17.0, W::Bold, INK, Align2::CENTER_CENTER);
                c.text(cx, 372.0, &up, 12.0, W::Regular, SUB2, Align2::CENTER_CENTER);
            }
        }

        // 折线
        let series: [(fn(&TrendSample) -> f32, Color32); 2] = if load {
            [(|s| s.cpu, YELLOW), (|s| s.mem, BLUE)]
        } else {
            [(|s| s.down, YELLOW), (|s| s.up, BLUE)]
        };
        let max = if load {
            100.0
        } else {
            let peak = cols.iter().flatten().map(|s| s.down.max(s.up)).fold(0.0f32, f32::max);
            (peak * 1.2).max(65_536.0)
        };
        for (get, color) in series {
            let pts: Vec<Pos2> = cols
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    s.map(|s| {
                        let x = 95.0 + i as f32 * 118.0;
                        let y = 420.0 - (get(&s) / max).clamp(0.0, 1.0) * 38.0;
                        c.at(x, y)
                    })
                })
                .collect();
            if pts.len() >= 2 {
                c.paint.add(egui::Shape::line(pts.clone(), Stroke::new(2.0 * c.k, color)));
            }
            for p in pts {
                c.paint.circle_filled(p, 3.2 * c.k, color);
            }
        }

        if let Some(i) = hovered {
            if let Some(s) = cols[i] {
                let cx = 95.0 + i as f32 * 118.0;
                let head = if i == COLS - 1 { "现在".to_string() } else { stamp(s.at) };
                let text = if load {
                    format!("{head}　CPU {:.0}% · 内存 {:.0}% · GPU {:.0}%", s.cpu, s.mem, s.gpu)
                } else {
                    format!("{head}　↓ {} · ↑ {}", format_speed(s.down as u64, d), format_speed(s.up as u64, d))
                };
                let w = c.text_w(&text, 12.0, W::Regular) + 22.0;
                let x = (cx - w * 0.5).clamp(26.0, 754.0 - w);
                c.round(x, 254.0, w, 26.0, 10.0, Color32::from_rgb(30, 33, 38));
                c.outline(x, 254.0, w, 26.0, 10.0, Stroke::new(1.0_f32, Color32::from_white_alpha(30)));
                c.text(x + w * 0.5, 267.0, &text, 12.0, W::Regular, Color32::from_rgb(227, 231, 236), Align2::CENTER_CENTER);
            }
        }
    }
}
