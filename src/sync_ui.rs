//! 多机同步的设置页：设置窗口最下面的「多端同步」。
//!
//! 一页包含原来的「同步」与「多端管理」：开关、连接、本机身份、所有设备、
//! 主设备与共享范围。数据层在 `sync.rs` 与各 `sync_*` 模块。

use eframe::egui::{self, RichText};

use crate::app::{App, Cmd};
use crate::extras::{setting_row, settings_card, settings_label};

impl App {
    /// 合并页：课表与多端同步共用同一个 NOS 地址和一枚 PAT。
    pub(crate) fn nos_sync_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (section, label) in [(0u8, "课表"), (1, "多端同步")] {
                let selected = self.nos_sync_section == section;
                let button = egui::Button::new(RichText::new(label).size(13.0)).selected(selected);
                if ui.add_sized([96.0, 30.0], button).clicked() {
                    self.nos_sync_section = section;
                }
            }
            ui.label(
                RichText::new("地址与令牌在「课表」分段设置，两段共用同一枚 PAT（勾选「读取日程 + 多机同步」）")
                    .size(11.0)
                    .color(egui::Color32::from_gray(150)),
            );
        });
        ui.separator();
        match self.nos_sync_section {
            0 => self.nos_settings(ui),
            _ => self.sync_settings(ui),
        }
    }

    pub(crate) fn sync_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        // 设备身份懒加载：只有真的打开这一页才生成 device.json。
        if self.sync_device.id.is_empty() {
            self.sync_device = crate::sync::load_device();
        }
        if self.sync_name_draft.is_none() {
            self.sync_name_draft = Some(self.sync_device.name.clone());
        }
        let mut pending: Vec<Cmd> = Vec::new();

        // ------------------------------------------------ 连接（与课表共用）
        settings_label(ui, "连接");
        settings_card(ui, |ui| {
            ui.small("服务器地址与访问令牌在「课表」分段设置，课表和多端同步共用同一份；令牌勾选「读取日程 + 多机同步」即可两用。");
            ui.horizontal(|ui| {
                if ui.button("立即同步").clicked() {
                    pending.push(Cmd::SyncNow);
                }
                if ui.button("忘记令牌").clicked() {
                    // 共用令牌：课表与同步一起清除。
                    crate::sync::secrets::delete();
                    self.nos_token_draft = Some(String::new());
                    pending.push(Cmd::SyncReconfigure);
                }
            });
            ui.add_space(3.0);
            status_block(ui, self.settings.sync_enabled, &self.sync);
        });

        // ------------------------------------------------ 开关
        settings_label(ui, "云同步");
        settings_card(ui, |ui| {
            let before = self.settings.sync_enabled;
            setting_row(
                ui,
                "启用同步",
                "接入 NOS 后，让多台 Windows 上的岛仔共用待办、饮水、番茄、日程与账号级设置；关闭时完全不联网",
                &mut self.settings.sync_enabled,
            );
            if self.settings.sync_enabled != before {
                self.settings.save();
                pending.push(Cmd::SyncReconfigure);
            }
            if self.settings.sync_enabled && self.settings.nos_base_url.trim().is_empty() {
                ui.small("还没有填服务器地址；先在「课表」分段填好并保存后才会开始同步。");
            }
        });

        // ------------------------------------------------ 本机
        settings_label(ui, "本机");
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("显示名称").size(13.0));
                ui.add(
                    egui::TextEdit::singleline(
                        self.sync_name_draft.as_mut().expect("设备名草稿已初始化"),
                    )
                    .desired_width(180.0)
                    .hint_text("书房台式 / 宿舍笔记本"),
                );
                if ui.button("保存名称").clicked() {
                    let name = self
                        .sync_name_draft
                        .clone()
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    match crate::sync::rename_device(&name) {
                        Ok(device) => {
                            self.sync_device = device;
                            self.sync_name_draft = Some(self.sync_device.name.clone());
                            self.say("设备名称已保存，下次同步时其他设备会看到新名字");
                            pending.push(Cmd::SyncReconfigure);
                        }
                        Err(error) => self.say(format!("保存名称失败：{error}")),
                    }
                }
            });
            ui.small(format!(
                "设备 id {} · Windows · 应用版本 {}",
                short_id(&self.sync_device.id),
                env!("CARGO_PKG_VERSION")
            ));
            ui.small("名称会显示在其他设备的列表里；设备 id 只在首次启动时生成，不随改名变化。");
        });

        // ------------------------------------------------ 所有设备
        // 主设备：显式设置优先，否则取最近活跃的那台（与服务端排序一致）。
        let primary = if !self.settings.sync_primary_device.trim().is_empty() {
            self.settings.sync_primary_device.clone()
        } else {
            self.sync
                .devices
                .iter()
                .find(|device| device.online)
                .map(|device| device.id.clone())
                .unwrap_or_default()
        };

        settings_label(ui, "所有设备");
        settings_card(ui, |ui| {
            if self.sync.devices.is_empty() {
                ui.small(if self.settings.sync_enabled {
                    "还没有连上服务器；连上后这里会列出登录同一账号的所有设备。"
                } else {
                    "同步未启用。填好上面的地址与令牌并保存后，这里就会出现设备。"
                });
                return;
            }
            for (index, device) in self.sync.devices.clone().iter().enumerate() {
                if index > 0 {
                    ui.separator();
                }
                let me = device.id == self.sync_device.id;
                let is_primary = !primary.is_empty() && device.id == primary;
                ui.horizontal(|ui| {
                    let color = if device.online {
                        egui::Color32::from_rgb(90, 190, 120)
                    } else {
                        egui::Color32::from_gray(150)
                    };
                    ui.label(RichText::new("●").size(11.0).color(color));
                    ui.label(RichText::new(device.name.as_str()).size(13.5));
                    if me {
                        ui.label(
                            RichText::new("本机")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(90, 170, 210)),
                        );
                    }
                    if is_primary {
                        ui.label(
                            RichText::new("主设备")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(182, 163, 245)),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    let status = if device.online {
                        "在线".to_string()
                    } else if device.last_seen.is_empty() {
                        "离线".to_string()
                    } else {
                        format!("最后活跃 {}", short_time(&device.last_seen))
                    };
                    ui.label(
                        RichText::new(status)
                            .size(11.0)
                            .color(egui::Color32::from_gray(150)),
                    );
                    if !device.platform.is_empty() {
                        ui.label(
                            RichText::new(device.platform.as_str())
                                .size(11.0)
                                .color(egui::Color32::from_gray(150)),
                        );
                    }
                    ui.label(
                        RichText::new(short_id(&device.id))
                            .size(11.0)
                            .color(egui::Color32::from_gray(150)),
                    );
                    if !me && !is_primary && ui.small_button("设为主设备").clicked() {
                        self.settings.sync_primary_device = device.id.clone();
                        self.settings.save();
                        self.say(format!("已指定「{}」负责提醒", device.name));
                    }
                });
                if is_primary && !self.settings.sync_primary_device.trim().is_empty() {
                    if ui.small_button("恢复为自动（最近活跃）").clicked() {
                        self.settings.sync_primary_device.clear();
                        self.settings.save();
                    }
                }
            }
        });

        // ------------------------------------------------ 共享范围
        settings_label(ui, "共享范围");
        settings_card(ui, |ui| {
            ui.small("会同步：待办、饮水、番茄、本机日程、账号级设置（皮肤、专注/饮水时长、页面顺序等）。");
            ui.small("不会同步：窗口位置与大小、显示器、置物架路径、通知监听、邮件与网盘凭据。");
            ui.small("当前版本已接入待办、饮水、番茄、本机日程与账号级设置；邮件与实时在场在后续版本。");
        });

        // ------------------------------------------------ 说明
        settings_label(ui, "说明");
        settings_card(ui, |ui| {
            ui.small("主设备负责弹待办与日程提醒；其他在线设备只更新角标，避免两个房间同时响。");
            ui.small("没指定主设备时，自动取最近活跃的那台；两台同时活跃时按设备 id 稳定兜底。");
            ui.small("要撤销某台设备的访问，请到 NOS「我的 → 账号 → 岛仔令牌」撤销它的令牌；本页不提供远程下线。");
            ui.small("重装系统或删掉数据目录会生成新的设备 id，旧设备会作为离线记录留在列表里。");
        });

        self.pending.extend(pending);
    }
}

/// 状态块：连接、首次同步、错误与游标。
fn status_block(ui: &mut egui::Ui, enabled: bool, snap: &crate::sync::Snapshot) {
    if !enabled {
        ui.small("同步未启用，岛仔不会发起任何同步请求。");
        return;
    }
    if let Some(error) = snap.error.as_deref() {
        ui.label(
            RichText::new(format!("连接失败：{error}"))
                .size(11.5)
                .color(egui::Color32::from_rgb(230, 120, 110)),
        );
        return;
    }
    if !snap.connected {
        ui.small("正在连接…");
        return;
    }
    if !snap.ready {
        ui.small("已连接 · 正在首次同步，完成后才会开始上传本机数据");
        return;
    }
    let who = if snap.user.trim().is_empty() {
        "已连接".to_string()
    } else {
        format!("已连接 · {}", snap.user)
    };
    ui.label(
        RichText::new(who)
            .size(11.5)
            .color(egui::Color32::from_rgb(90, 170, 120)),
    );
    ui.small(format!(
        "游标 {} · 待上传 {} 条 · 待应用 {} 条 · 最近同步 {}",
        snap.cursor,
        snap.pending,
        snap.inbox,
        if snap.last_ok > 0 {
            short_time_epoch(snap.last_ok)
        } else {
            "尚未成功".into()
        }
    ));
    if snap.skew_ms.abs() >= 5 * 60 * 1000 {
        ui.label(
            RichText::new(format!(
                "本机与服务端时间相差约 {} 分钟，番茄倒计时可能不准",
                snap.skew_ms / 60_000
            ))
            .size(11.0)
            .color(egui::Color32::from_rgb(230, 175, 80)),
        );
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `2026-11-18T14:50:40.262Z` → `11-18 22:50`（按本机时区）。
fn short_time(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| raw.to_string())
}

fn short_time_epoch(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "刚刚".into())
}
