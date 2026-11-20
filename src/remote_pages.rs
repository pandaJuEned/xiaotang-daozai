//! 异地页：其他在线设备的天气、监控、番茄与提醒数（第 11 个功能页面）。
//!
//! 数据全部来自 `sync::Snapshot.devices` 的 presence 心跳（90 秒在线判定），
//! 本模块不发请求、不落盘；同一台设备的天气与监控合并到同一张卡片。
//! 可用横向区间是 42..376，纵向 34 以下才是页面自己的位置。

use eframe::egui::Color32;

use crate::app::{App, Env, REMOTE_PAGE};
use crate::paint::{self, Card, H};
use crate::theme::{self, W};

const LEFT: f32 = 42.0;
const RIGHT: f32 = 376.0;
const ROW_H: f32 = 82.0;
const PER_PAGE: usize = 2;

impl App {
    /// 第 11 页：异地——同一台设备的天气、监控、电池与提醒合并显示。
    pub(crate) fn page_remote(&mut self, c: &Card, env: &Env) {
        let _ = env;
        if !self.settings.sync_enabled {
            self.remote_page_hint(
                c,
                "异地信息需要多机同步",
                "在「设置 → 课表与多端」启用同步后，这里显示其他设备的天气与监控",
                "remote-enable",
                15,
                "去启用",
            );
            return;
        }
        let devices: Vec<(String, crate::sync::PresenceOut)> = self
            .sync
            .devices
            .iter()
            .filter(|device| device.online && device.id != self.sync_device.id)
            .filter_map(|device| device.presence_data().map(|presence| (device.name.clone(), presence)))
            .take(8)
            .collect();
        if devices.is_empty() {
            c.text(209.5, 88.0, "暂时没有其他在线设备", 13.0, W::Bold, theme::WHITE, H::Center);
            c.text(
                209.5,
                108.0,
                "两台都启用同步并保持在线后，这里合并显示天气、监控、番茄与提醒数",
                10.5,
                W::Regular,
                theme::GRAY,
                H::Center,
            );
            return;
        }
        let pages = devices.len().div_ceil(PER_PAGE).max(1);
        if self.remote_group >= pages { self.remote_group = pages - 1; }
        let start = self.remote_group * PER_PAGE;
        let shown: Vec<_> = devices.iter().skip(start).take(PER_PAGE).collect();
        let mut y = 34.0;
        for (index, (name, presence)) in shown.iter().enumerate() {
            let height = ROW_H - 6.0;
            c.fill(LEFT, y, RIGHT - LEFT, height, 8.0, theme::CARD);
            let hit = c.hit(LEFT, y, RIGHT - LEFT, height, ("remote-row", index as u32));
            let (hover, _) = c.hover_t(&hit);
            if hover > 0.0 {
                c.fill(LEFT, y, RIGHT - LEFT, height, 8.0, Color32::from_white_alpha((hover * 12.0) as u8));
            }
            // 第一行：设备名 + 状态（提醒 > 番茄 > 在线）。
            c.text(
                LEFT + 12.0,
                y + 15.0,
                &c.fit(name, 11.5, W::Semibold, 200.0),
                11.5,
                W::Semibold,
                theme::WHITE,
                H::Left,
            );
            let status = if !presence.messages.is_empty() {
                let text = presence.messages.iter().take(2)
                    .map(|message| format!("{} {}", message.source, message.count))
                    .collect::<Vec<_>>().join(" · ");
                (format!("消息 {text}"), Color32::from_rgb(90, 190, 120), W::Semibold)
            } else if presence.alerts > 0 {
                (format!("有 {} 条提醒", presence.alerts), Color32::from_rgb(255, 176, 46), W::Semibold)
            } else if presence.focus.as_ref().is_some_and(|focus| focus.running) {
                let focus = presence.focus.as_ref().expect("已判断存在");
                (
                    format!(
                        "番茄 {} {}′",
                        if focus.phase == "rest" { "休息" } else { "专注" },
                        focus.remaining / 60
                    ),
                    theme::CYAN,
                    W::Semibold,
                )
            } else {
                ("在线 · 无未读".into(), theme::DIM, W::Regular)
            };
            c.text(RIGHT - 12.0, y + 15.0, &status.0, 10.5, status.2, status.1, H::Right);
            // 第二行：天气。
            let weather = match presence.weather.as_ref() {
                Some(weather) => format!(
                    "{}° {} · {} · {} 分钟前",
                    weather.temp.round() as i32,
                    weather.text,
                    weather.city,
                    (chrono::Local::now().timestamp() - weather.at).max(0) / 60
                ),
                None => "天气：对方未上报".into(),
            };
            c.text(
                LEFT + 12.0,
                y + 31.0,
                &c.fit(&weather, 10.5, W::Regular, 312.0),
                10.5,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            // 第三行：监控 + 电池。
            let metrics = match presence.monitor.as_ref() {
                Some(monitor) => {
                    let value = |value: Option<f32>, suffix: &str| {
                        value
                            .map(|value| format!("{value:.0}{suffix}"))
                            .unwrap_or_else(|| "--".into())
                    };
                    format!(
                        "CPU {} · 内存 {} · 硬盘 {} · 温度 {} · 显卡 {}",
                        value(monitor.cpu, "%"),
                        value(monitor.mem, "%"),
                        value(monitor.disk, "%"),
                        value(monitor.temp, "°"),
                        value(monitor.gpu, "%"),
                    )
                }
                None => "监控：对方未上报".into(),
            };
            let battery = match presence.battery.as_ref() {
                Some(battery) => match battery.percent {
                    Some(percent) => {
                        format!(" · 电池 {}%{}", percent, if battery.charging { " 充电中" } else { "" })
                    }
                    None if battery.ac_online => " · 电源供电".into(),
                    None => " · 未接电源".into(),
                },
                None => String::new(),
            };
            c.text(
                LEFT + 12.0,
                y + 47.0,
                &c.fit(&format!("{metrics}{battery}"), 10.0, W::Regular, 312.0),
                10.0,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            // 第四行：正在播放的媒体。
            let music = match presence.music.as_ref() {
                Some(music) => format!(
                    "{}播放：{} · {} · {}",
                    if music.playing { "" } else { "暂停 " },
                    music.source,
                    music.title,
                    music.artist,
                ),
                None => "音乐：未播放".into(),
            };
            c.text(
                LEFT + 12.0,
                y + 63.0,
                &c.fit(&music, 9.5, W::Regular, 312.0),
                9.5,
                W::Regular,
                if presence.music.is_some() { theme::CYAN } else { theme::DIM },
                H::Left,
            );
            if c.live && hit.clicked() {
                self.remote_monitor_open = true;
            }
            y += ROW_H;
        }
        if pages > 1 {
            let (prev, prev_body) = paint::capsule(c, 150.0, y + 4.0, 44.0, 20.0, "remote-prev", theme::BTN, theme::BTN_HOVER);
            prev_body.text(172.0, y + 18.0, "◀", 9.5, W::Semibold, theme::WHITE, H::Center);
            let (next, next_body) = paint::capsule(c, 226.0, y + 4.0, 44.0, 20.0, "remote-next", theme::BTN, theme::BTN_HOVER);
            next_body.text(248.0, y + 18.0, "▶", 9.5, W::Semibold, theme::WHITE, H::Center);
            c.text(209.5, y + 18.0, &format!("{}/{}", self.remote_group + 1, pages), 9.5, W::Regular, theme::GRAY, H::Center);
            if c.live && prev.clicked() && self.remote_group > 0 { self.remote_group -= 1; }
            if c.live && next.clicked() && self.remote_group + 1 < pages { self.remote_group += 1; }
        } else {
            c.text(209.5, y + 18.0, "点卡片打开异地监控窗口；只报来源与数量，不显示内容", 9.0, W::Regular, theme::DIM, H::Center);
        }
    }

    /// 引导卡：说明 + 「去打开」胶囊。
    fn remote_page_hint(
        &mut self,
        c: &Card,
        title: &str,
        detail: &str,
        id: &str,
        tab: usize,
        action: &str,
    ) {
        c.text(209.5, 84.0, title, 13.0, W::Bold, theme::WHITE, H::Center);
        c.text(209.5, 104.0, detail, 10.5, W::Regular, theme::GRAY, H::Center);
        if c.live {
            let (button, body) =
                paint::capsule(c, 162.0, 134.0, 95.0, 24.0, id, theme::BTN, theme::BTN_HOVER);
            body.text(209.5, 150.0, action, 10.0, W::Semibold, theme::WHITE, H::Center);
            if button.clicked() {
                self.settings_tab = tab;
                self.open_settings();
            }
        }
    }
}
