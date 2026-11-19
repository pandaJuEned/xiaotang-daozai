//! 异地两页：其他在线设备的天气（11）与监控/在场（12）。
//!
//! 数据全部来自 `sync::Snapshot.devices` 的 presence 心跳（90 秒在线判定），
//! 本模块不发请求、不落盘；同步关闭时两页显示引导卡片。
//! 可用横向区间是 42..376，纵向 34 以下才是页面自己的位置。

use eframe::egui::Color32;

use crate::app::{App, Env, REMOTE_MONITOR_PAGE, REMOTE_WEATHER_PAGE};
use crate::paint::{self, Card, H};
use crate::theme::{self, W};

const LEFT: f32 = 42.0;
const RIGHT: f32 = 376.0;
const ROW_H: f32 = 46.0;

impl App {
    /// 第 11 页：异地天气。
    pub(crate) fn page_remote_weather(&mut self, c: &Card, env: &Env) {
        let _ = env;
        if !self.settings.sync_enabled {
            self.remote_page_hint(
                c,
                "异地天气需要多机同步",
                "在「设置 → 课表与多端」启用同步后，这里显示其他设备的当地天气",
                "remote-weather-enable",
                15,
                "去启用",
            );
            return;
        }
        let remote = self.remote_weather();
        if remote.is_empty() {
            c.text(209.5, 92.0, "暂时没有其他设备的天气", 13.0, W::Bold, theme::WHITE, H::Center);
            c.text(
                209.5,
                112.0,
                "两台都启用同步并保持在线后，这里会列出它们的城市与温度",
                10.5,
                W::Regular,
                theme::GRAY,
                H::Center,
            );
            return;
        }
        let mut y = 40.0;
        for (index, (name, weather)) in remote.iter().take(4).enumerate() {
            let height = ROW_H - 6.0;
            c.fill(LEFT, y, RIGHT - LEFT, height, 8.0, theme::CARD);
            let hit = c.hit(LEFT, y, RIGHT - LEFT, height, ("remote-weather-row", index as u32));
            let (hover, _) = c.hover_t(&hit);
            if hover > 0.0 {
                c.fill(LEFT, y, RIGHT - LEFT, height, 8.0, Color32::from_white_alpha((hover * 12.0) as u8));
            }
            c.circle(LEFT + 16.0, y + 14.0, 3.0, theme::CYAN);
            c.text(
                LEFT + 28.0,
                y + 17.0,
                &c.fit(name, 11.5, W::Semibold, 168.0),
                11.5,
                W::Semibold,
                theme::WHITE,
                H::Left,
            );
            c.text(
                LEFT + 28.0,
                y + 34.0,
                &c.fit(&weather.city, 10.0, W::Regular, 168.0),
                10.0,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            c.text(
                RIGHT - 96.0,
                y + 24.0,
                &format!("{}°", weather.temp.round() as i32),
                18.0,
                W::Bold,
                theme::WHITE,
                H::Right,
            );
            c.text(
                RIGHT - 12.0,
                y + 17.0,
                &c.fit(&weather.text, 10.5, W::Regular, 92.0),
                10.5,
                W::Regular,
                theme::GRAY,
                H::Right,
            );
            let age = (chrono::Local::now().timestamp() - weather.at).max(0) / 60;
            c.text(
                RIGHT - 12.0,
                y + 34.0,
                &format!("{} 分钟前", age),
                9.5,
                W::Regular,
                theme::DIM,
                H::Right,
            );
            if c.live && hit.clicked() {
                self.settings_tab = 12;
                self.open_settings();
            }
            y += ROW_H;
        }
        c.text(
            209.5,
            (y + 12.0).max(216.0),
            "点卡片打开「设置 → 天气」；同城设备自动隐藏",
            9.5,
            W::Regular,
            theme::DIM,
            H::Center,
        );
    }

    /// 第 12 页：异地监控与在场（通知只报数量与来源，不报内容）。
    pub(crate) fn page_remote_monitor(&mut self, c: &Card, env: &Env) {
        let _ = env;
        if !self.settings.show_remote_monitor {
            self.remote_page_hint(
                c,
                "异地监控已关闭",
                "在「设置 → 系统监控 → 显示」打开「查看其他设备的系统监控」",
                "remote-monitor-enable",
                13,
                "去打开",
            );
            return;
        }
        if !self.settings.sync_enabled {
            self.remote_page_hint(
                c,
                "异地监控需要多机同步",
                "在「设置 → 课表与多端」启用同步后，这里显示其他设备的读数",
                "remote-monitor-sync",
                15,
                "去启用",
            );
            return;
        }
        let devices: Vec<(String, crate::sync::PresenceMonitor, u32, Option<crate::sync::PresenceFocus>)> =
            self.sync
                .devices
                .iter()
                .filter(|device| device.online && device.id != self.sync_device.id)
                .filter_map(|device| {
                    let presence = device.presence_data()?;
                    let monitor = presence.monitor?;
                    Some((device.name.clone(), monitor, presence.alerts, presence.focus))
                })
                .take(4)
                .collect();
        if devices.is_empty() {
            c.text(209.5, 92.0, "暂时没有其他在线设备", 13.0, W::Bold, theme::WHITE, H::Center);
            c.text(
                209.5,
                112.0,
                "两台都启用同步并保持在线后，这里会显示 CPU、内存、温度与通知数",
                10.5,
                W::Regular,
                theme::GRAY,
                H::Center,
            );
            return;
        }
        let mut y = 40.0;
        for (index, (name, monitor, alerts, focus)) in devices.iter().enumerate() {
            let height = ROW_H - 6.0;
            c.fill(LEFT, y, RIGHT - LEFT, height, 8.0, theme::CARD);
            let hit = c.hit(LEFT, y, RIGHT - LEFT, height, ("remote-monitor-row", index as u32));
            let (hover, _) = c.hover_t(&hit);
            if hover > 0.0 {
                c.fill(LEFT, y, RIGHT - LEFT, height, 8.0, Color32::from_white_alpha((hover * 12.0) as u8));
            }
            let value = |value: Option<f32>, suffix: &str| {
                value
                    .map(|value| format!("{value:.0}{suffix}"))
                    .unwrap_or_else(|| "--".into())
            };
            let metrics = format!(
                "CPU {} · 内存 {} · 硬盘 {} · 温度 {} · 显卡 {}",
                value(monitor.cpu, "%"),
                value(monitor.mem, "%"),
                value(monitor.disk, "%"),
                value(monitor.temp, "°"),
                value(monitor.gpu, "%"),
            );
            c.text(
                LEFT + 14.0,
                y + 17.0,
                &c.fit(name, 11.5, W::Semibold, 220.0),
                11.5,
                W::Semibold,
                theme::WHITE,
                H::Left,
            );
            c.text(
                LEFT + 14.0,
                y + 35.0,
                &c.fit(&metrics, 10.0, W::Regular, 244.0),
                10.0,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            if *alerts > 0 {
                c.text(
                    RIGHT - 12.0,
                    y + 17.0,
                    &format!("有 {} 条提醒", alerts),
                    10.5,
                    W::Semibold,
                    Color32::from_rgb(255, 176, 46),
                    H::Right,
                );
            } else if let Some(focus) = focus {
                if focus.running {
                    c.text(
                        RIGHT - 12.0,
                        y + 17.0,
                        &format!(
                            "番茄 {} {}′",
                            if focus.phase == "rest" { "休息" } else { "专注" },
                            focus.remaining / 60
                        ),
                        10.5,
                        W::Semibold,
                        theme::CYAN,
                        H::Right,
                    );
                } else {
                    c.text(RIGHT - 12.0, y + 17.0, "番茄暂停", 10.0, W::Regular, theme::DIM, H::Right);
                }
            } else {
                c.text(RIGHT - 12.0, y + 17.0, "在线", 10.0, W::Regular, theme::DIM, H::Right);
            }
            if c.live && hit.clicked() {
                self.remote_monitor_open = true;
            }
            y += ROW_H;
        }
        c.text(
            209.5,
            (y + 12.0).max(216.0),
            "数值每 30 秒随心跳刷新；点卡片打开异地监控窗口",
            9.5,
            W::Regular,
            theme::DIM,
            H::Center,
        );
    }

    /// 两页共用的引导卡：说明 + 「去打开」胶囊。
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
