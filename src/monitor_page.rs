//! 岛上「系统监控」页（第 10 个功能页面）。
//!
//! 版式参照 `system-monitor-ui/index.html` 原型，按岛体 419×236 重排：
//! 左侧 CPU 主指标、右上 2×2 指标格、底部六列趋势 + 双折线。
//!
//! **可用横向区间是 42..376**：外壳在 x 8..34 与 x 385..411 画翻页箭头，
//! 内容越过去就会被箭头压住。纵向 34 以下才是页面自己的位置。
//!
//! 数据全部来自 `Env.monitor` 与 `App::monitor_trend`，本模块不读系统、不落盘。

use eframe::egui::{self, Color32, Shape, Stroke};

use crate::app::{App, Env};
use crate::paint::{Card, H};
use crate::theme::{self, W};

/// 底部概览列数，与仪表盘窗口保持一致。
const COLS: usize = 6;

const BLUE: Color32 = Color32::from_rgb(61, 158, 255);
const YELLOW: Color32 = Color32::from_rgb(255, 176, 46);
const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
const PURPLE: Color32 = Color32::from_rgb(191, 90, 242);
const CYAN: Color32 = Color32::from_rgb(100, 210, 255);
const HOT: Color32 = Color32::from_rgb(255, 93, 93);
const HAIR: Color32 = Color32::from_rgb(46, 46, 50);

/// 内容左右边界（避开翻页箭头）。
const LEFT: f32 = 42.0;
const RIGHT: f32 = 376.0;

/// 指标格与下方电源条的几何。
const CELL_W: f32 = 104.0;
const CELL_H: f32 = 24.0;
const CELL_X: [f32; 2] = [160.0, 272.0];
const CELL_Y: [f32; 2] = [28.0, 56.0];
const POWER_Y: f32 = 86.0;

fn load_color(v: f32) -> Color32 {
    if v < 40.0 {
        BLUE
    } else if v < 75.0 {
        YELLOW
    } else {
        HOT
    }
}

fn net_color(bps: f32) -> Color32 {
    if bps < 1_048_576.0 {
        BLUE
    } else if bps < 4_194_304.0 {
        YELLOW
    } else {
        HOT
    }
}

/// 页内用的紧凑速率：`1.2M` / `300K` / `0`。
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

fn state_text(cpu: Option<f32>) -> &'static str {
    match cpu {
        Some(v) if v < 30.0 => "负载较低",
        Some(v) if v < 65.0 => "负载正常",
        Some(_) => "负载较高",
        None => "等待采样",
    }
}

fn stamp(at: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(at, 0)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

/// 24 单位网格的 CPU 图标（仪表盘与设置页实况卡共用）。
pub(crate) fn cpu_glyph(c: &Card, cx: f32, cy: f32, size: f32, color: Color32) {
    let s = size / 24.0;
    let px = |v: f32| cx + (v - 12.0) * s;
    let py = |v: f32| cy + (v - 12.0) * s;
    let w = 1.7 * s;
    let ring = |x: f32, y: f32, ww: f32, hh: f32, r: f32| {
        let rect = egui::Rect::from_min_max(c.pos(px(x), py(y)), c.pos(px(x + ww), py(y + hh)));
        c.painter
            .rect(rect, r * s, Color32::TRANSPARENT, Stroke::new(w, color), egui::StrokeKind::Inside);
    };
    ring(5.0, 5.0, 14.0, 14.0, 2.5);
    ring(9.5, 9.5, 5.0, 5.0, 1.0);
    for v in [9.0, 15.0] {
        for (a, b, cc, d) in [(v, 2.5, v, 5.0), (v, 19.0, v, 21.5), (2.5, v, 5.0, v), (19.0, v, 21.5, v)] {
            c.painter
                .line_segment([c.pos(px(a), py(b)), c.pos(px(cc), py(d))], Stroke::new(w, color));
        }
    }
}

impl App {
    /// 系统监控页。`c.live` 为 false 时是设置页里的预览，不做交互。
    pub(crate) fn page_monitor(&mut self, c: &Card, env: &Env) {
        // 右上角刷新：清空曲线重开一个采样窗口。
        let refresh = c.hit(330.0, 6.0, 32.0, 22.0, "monitor-page-refresh");
        if refresh.hovered() {
            c.painter.rect_filled(c.rect(330.0, 6.0, 32.0, 22.0), 7.0, Color32::from_white_alpha(12));
        }
        let refresh_col = if refresh.hovered() { theme::WHITE } else { theme::GRAY };
        c.text(346.0, 22.0, "刷新", 10.5, W::Regular, refresh_col, H::Center);
        if refresh.clicked() {
            self.monitor_trend.clear();
        }
        // 异地设备统一从「异地」功能页进入，本页不再显示「异地 N」胶囊。

        let Some(m) = env.monitor else {
            c.text(209.5, 96.0, "还没有监控数据", 13.0, W::Bold, theme::WHITE, H::Center);
            c.text(209.5, 116.0, "在「设置 → 系统监控」打开采集后这里会显示实时读数", 10.5, W::Regular, theme::GRAY, H::Center);
            if c.live {
                let (r, b) = crate::paint::capsule(c, 162.0, 134.0, 95.0, 24.0, "monitor-page-enable", theme::BTN, theme::BTN_HOVER);
                b.text(209.5, 150.0, "去打开", 10.0, W::Semibold, theme::WHITE, H::Center);
                if r.clicked() {
                    self.settings_tab = 13;
                    self.open_settings();
                }
            }
            return;
        };

        /* ---------- 左侧 CPU 主指标 ---------- */
        cpu_glyph(c, 52.0, 56.0, 22.0, BLUE);
        let cpu = pct(m.cpu_percent).trim_end_matches('%').to_string();
        let nw = c.text(66.0, 74.0, &cpu, 30.0, W::Bold, theme::WHITE, H::Left);
        c.text(66.0 + nw + 2.0, 74.0, "%", 12.5, W::Bold, theme::GRAY, H::Left);

        let state = state_text(m.cpu_percent);
        let freq = m
            .cpu_mhz
            .map(|v| format!(" · {:.2} GHz", v as f32 / 1000.0))
            .unwrap_or_default();
        c.text(LEFT, 90.0, &format!("{state}{freq}"), 9.5, W::Semibold, theme::GRAY, H::Left);
        // 温度和机器名分两行：温度强调显示，机器名独占下一行，长名字截断在左栏内。
        // 桌面平台没有 ACPI 热区，此时退回显卡温度并加 `GPU` 前缀，避免把显卡温度当成机身温度。
        let (temperature, color) = match m.temperature_c {
            Some(v) => (format!("{v:.0}°C"), load_color(v)),
            None => match m.gpu.temp_c {
                Some(v) => (format!("GPU {v:.0}°C"), load_color(v)),
                None => ("温度 --".to_string(), theme::DIM),
            },
        };
        c.text(LEFT, 104.0, &temperature, 10.5, W::Bold, color, H::Left);
        let machine = c.fit(&crate::monitor::machine_name(), 8.5, W::Regular, CELL_X[0] - LEFT - 2.0);
        c.text(LEFT, 116.0, &machine, 8.5, W::Regular, theme::DIM, H::Left);

        /* ---------- 右侧 2×2 指标格 + 电源条 ---------- */
        let mem_sub = format!(
            "{:.1}/{:.1}G",
            m.mem_used_bytes as f64 / 1_073_741_824.0,
            m.mem_total_bytes as f64 / 1_073_741_824.0
        );
        let disk_sub = match (m.disk_read_mbps, m.disk_write_mbps) {
            (Some(r), Some(w)) => format!("读{r:.0}·写{w:.0}"),
            _ => "读写 --".to_string(),
        };
        let gpu_sub = m.gpu.brief().unwrap_or_default();
        let cells: [(String, String, String, Color32, Color32); 4] = [
            ("内存".into(), pct(m.mem_percent()), mem_sub, BLUE, theme::WHITE),
            ("GPU".into(), pct(m.gpu_percent), gpu_sub, GREEN, theme::WHITE),
            ("硬盘".into(), pct(m.disk_percent), disk_sub, PURPLE, theme::WHITE),
            (
                "网速".into(),
                format!("↓{}", brief(m.down_bps)),
                format!("↑{}", brief(m.up_bps)),
                CYAN,
                theme::WHITE,
            ),
        ];
        for (i, (label, value, sub, tint, value_color)) in cells.into_iter().enumerate() {
            let (x, y) = (CELL_X[i % 2], CELL_Y[i / 2]);
            c.painter
                .rect_filled(c.rect(x, y, CELL_W, CELL_H), 8.0, Color32::from_white_alpha(10));
            c.text(x + 8.0, y + 8.0, &label, 8.0, W::Regular, theme::DIM, H::Left);
            c.text(x + 8.0, y + 20.0, &value, 12.0, W::Bold, value_color, H::Left);
            if !sub.is_empty() {
                let sub = c.fit(&sub, 7.0, W::Regular, 44.0);
                c.text(x + CELL_W - 8.0, y + 20.0, &sub, 7.0, W::Regular, tint, H::Right);
            }
        }
        let p = &m.power;
        let ac = p.ac_online.map(|v| if v { "已接入" } else { "未接入" }).unwrap_or("--");
        let battery = p.battery_percent.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "--".into());
        let remaining = p.remaining_wh.map(|v| format!("{v:.1}Wh")).unwrap_or_else(|| "--".into());
        let flow = p
            .charge_w
            .map(|v| format!("充 {v:.1}W"))
            .or_else(|| p.discharge_w.map(|v| format!("放 {v:.1}W")))
            .unwrap_or_else(|| "充/放 --".into());
        let input = p.ac_power_w.map(|v| format!("入 {v:.0}W")).unwrap_or_else(|| "入 --".into());
        let power_text = c.fit(
            &format!("电源 · 交流{ac} · 电池{battery} · 余{remaining} · {flow} · {input}"),
            9.0,
            W::Regular,
            CELL_W * 2.0 + 4.0,
        );
        c.painter.rect_filled(
            c.rect(CELL_X[0], POWER_Y, CELL_W * 2.0 + 12.0, 22.0),
            8.0,
            Color32::from_white_alpha(10),
        );
        c.text(CELL_X[0] + 8.0, POWER_Y + 12.0, &power_text, 9.0, W::Regular, theme::GRAY, H::Left);

        /* ---------- 分隔线 ---------- */
        c.painter
            .line_segment([c.pos(LEFT, 122.0), c.pos(RIGHT, 122.0)], Stroke::new(1.0_f32, HAIR));

        /* ---------- 翻页：负载 / 网络 ---------- */
        let load = self.monitor_page == 0;
        for (i, (label, target)) in [("负载", 0usize), ("网络", 1usize)].into_iter().enumerate() {
            // 从左到右：负载、网络；网络贴右边界。
            let right = RIGHT - (1 - i) as f32 * 38.0;
            let w = c.text_w(label, 9.0, W::Regular);
            let hit = c.hit(right - w - 6.0, 124.0, w + 12.0, 14.0, ("monitor-page-tab", i));
            let on = self.monitor_page == target;
            let color = if on {
                theme::WHITE
            } else if hit.hovered() {
                theme::GRAY
            } else {
                theme::DIM
            };
            c.text(right, 135.0, label, 9.0, if on { W::Semibold } else { W::Regular }, color, H::Right);
            if hit.clicked() {
                self.monitor_page = target;
            }
        }

        /* ---------- 六列趋势 ---------- */
        let picked = self.monitor_trend.pick(COLS);
        let mut cols: [Option<crate::monitor::TrendSample>; COLS] = [None; COLS];
        let start = COLS.saturating_sub(picked.len());
        for (i, s) in picked.iter().enumerate() {
            cols[start + i] = Some(*s);
        }
        let slot = (RIGHT - LEFT) / COLS as f32;
        let center = |i: usize| LEFT + slot * (i as f32 + 0.5);

        for (i, sample) in cols.iter().enumerate() {
            let cx = center(i);
            let is_now = i == COLS - 1 && sample.is_some();
            let time = if is_now {
                "现在".to_string()
            } else {
                sample.map(|s| stamp(s.at)).unwrap_or_else(|| "--:--".into())
            };
            c.text(cx, 152.0, &time, 8.5, W::Regular, if is_now { theme::WHITE } else { theme::DIM }, H::Center);
            let s = sample.unwrap_or_default();
            let has = sample.is_some();
            let (big, small, dot) = if load {
                (
                    if has { format!("{:.0}%", s.cpu) } else { "--".into() },
                    if has { format!("{:.0}%", s.mem) } else { "--".into() },
                    load_color(s.cpu),
                )
            } else {
                (
                    if has { brief(s.down as u64) } else { "--".into() },
                    if has { brief(s.up as u64) } else { "--".into() },
                    net_color(s.down),
                )
            };
            c.painter.circle_filled(c.pos(cx - 13.0, 166.0), 2.0, dot);
            c.text(cx + 2.0, 170.0, &big, 12.0, W::Bold, theme::WHITE, H::Center);
            c.text(cx, 182.0, &small, 8.5, W::Regular, theme::DIM, H::Center);
        }

        // 折线：两条序列共用一条时间轴，与上方六列对齐。
        let max = if load {
            100.0
        } else {
            let peak = cols.iter().flatten().map(|s| s.down.max(s.up)).fold(0.0f32, f32::max);
            (peak * 1.2).max(65_536.0)
        };
        let series: [(fn(&crate::monitor::TrendSample) -> f32, Color32); 2] = if load {
            [(|s| s.cpu, YELLOW), (|s| s.mem, BLUE)]
        } else {
            [(|s| s.down, YELLOW), (|s| s.up, BLUE)]
        };
        for (get, color) in series {
            let pts: Vec<egui::Pos2> = cols
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    s.map(|s| {
                        let y = 214.0 - (get(&s) / max).clamp(0.0, 1.0) * 28.0;
                        c.pos(center(i), y)
                    })
                })
                .collect();
            if pts.len() >= 2 {
                c.painter.add(Shape::line(pts.clone(), Stroke::new(1.6_f32, color)));
            }
            for p in pts {
                c.painter.circle_filled(p, 2.2, color);
            }
        }

        // 图例：靠右两枚色点 + 名称。
        let legend: [(&str, Color32); 2] = if load {
            [("CPU", YELLOW), ("内存", BLUE)]
        } else {
            [("下行", YELLOW), ("上行", BLUE)]
        };
        let y = 224.0;
        let w0 = c.text_w(legend[0].0, 8.0, W::Regular);
        let w1 = c.text_w(legend[1].0, 8.0, W::Regular);
        c.text(RIGHT, y, legend[1].0, 8.0, W::Regular, theme::DIM, H::Right);
        c.painter.circle_filled(c.pos(RIGHT - w1 - 7.0, y - 3.0), 2.2, legend[1].1);
        let x1 = RIGHT - w1 - 7.0 - 14.0;
        c.text(x1, y, legend[0].0, 8.0, W::Regular, theme::DIM, H::Right);
        c.painter.circle_filled(c.pos(x1 - w0 - 7.0, y - 3.0), 2.2, legend[0].1);
    }
}
