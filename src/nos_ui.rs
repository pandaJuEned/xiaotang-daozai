//! 课表（NOS）的设置层：设置窗口里的「课表」页签。
//!
//! 数据层在 `nos.rs`：后台按间隔拉取服务端已展开好的事件流；本模块负责两件事：
//! - 把 NOS Web 端当作**独立服务**控制启停（进程管理在 `nos_server.rs`）；
//! - 配置连接信息、显示开关与今日课程一览。
//!
//! 令牌不落盘配置文件，只进 Windows 凭据管理器；启动本地服务时同一份令牌会作为
//! `NOS_API_TOKEN` 注入 uvicorn 进程，服务端与客户端天然对齐。

use eframe::egui::{self, RichText};

use crate::app::{App, Cmd};
use crate::extras::{setting_row, settings_card, settings_label};

impl App {
    /// 设置页签「课表」：本地服务控制、连接服务器、开关模块与常驻栏显示。
    pub(crate) fn nos_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut pending: Vec<Cmd> = Vec::new();
        let status = self.nos_server_status.clone();
        let note = self.nos_server_note.clone();
        let local_url = self.nos_server_config().base_url();
        let log_path = crate::model::config_dir().join(crate::nos_server::LOG_FILE);

        // ------------------------------------------------ 独立的 NOS Web 服务
        settings_label(ui, "NOS Web 服务");
        settings_card(ui, |ui| {
            setting_row(ui, "由岛仔管理本机服务",
                "把 NOS Web 端作为独立的 uvicorn 进程启动 / 停止；关闭时按下面的服务器地址连接一台现成的服务",
                &mut self.settings.nos_server_enabled);
            if !self.settings.nos_server_enabled {
                ui.small("关闭时岛仔只是客户端，不会碰任何本机进程；适合连接已经部署在服务器上的 NOS Web。");
                return;
            }
            ui.separator();
            setting_row(ui, "随岛仔启动自动运行",
                "岛仔启动时自动拉起服务；退出岛仔时进程由系统一并收走，不留后台残留",
                &mut self.settings.nos_server_auto_start);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Web 目录").size(13.0));
                ui.add(egui::TextEdit::singleline(&mut self.settings.nos_server_dir)
                    .desired_width(250.0)
                    .hint_text("NOS 仓库的 web 目录，例如 E:/dev/NOS/web"));
                if ui.button("自动探测").clicked() { pending.push(Cmd::NosServerDetect); }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Python").size(13.0));
                ui.add(egui::TextEdit::singleline(&mut self.settings.nos_server_python)
                    .desired_width(340.0)
                    .hint_text("留空自动使用 web/venv，或 PATH 上的 python"));
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("监听地址").size(13.0));
                ui.add(egui::TextEdit::singleline(&mut self.settings.nos_server_host).desired_width(140.0));
                ui.label(RichText::new("端口").size(13.0));
                ui.add(egui::DragValue::new(&mut self.settings.nos_server_port).range(1..=65535).speed(1.0));
            });
            ui.separator();
            let gray = egui::Color32::from_gray(150);
            let color = match status.phase {
                crate::nos_server::Phase::Running => egui::Color32::from_rgb(90, 190, 120),
                crate::nos_server::Phase::External => egui::Color32::from_rgb(90, 170, 210),
                crate::nos_server::Phase::Starting => egui::Color32::from_rgb(230, 175, 80),
                crate::nos_server::Phase::Unhealthy => egui::Color32::from_rgb(230, 120, 110),
                crate::nos_server::Phase::Stopped => gray,
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("● {}", status.label())).size(12.5).color(color));
                if let Some(pid) = status.pid {
                    ui.label(RichText::new(format!("PID {pid}")).size(11.5).color(gray));
                }
                ui.label(RichText::new(&status.detail).size(11.5).color(gray));
            });
            if let Some(note) = note.as_ref() {
                let note_color = if note.contains("失败") || note.contains("无法") || note.contains("未就绪") || note.contains("退出") {
                    egui::Color32::from_rgb(230,120,110)
                } else {
                    egui::Color32::from_rgb(90,170,120)
                };
                ui.label(RichText::new(note).size(11.5).color(note_color));
            }
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!status.healthy(), |ui| {
                    if ui.button("启动").clicked() { pending.push(Cmd::NosServerStart); }
                });
                if ui.button("停止").clicked() { pending.push(Cmd::NosServerStop); }
                if ui.button("重启").clicked() { pending.push(Cmd::NosServerRestart); }
            });
            ui.horizontal(|ui| {
                if ui.button("打开 Web").clicked() { pending.push(Cmd::NosServerOpenWeb); }
                if ui.button("查看日志").clicked() { pending.push(Cmd::NosServerOpenLog); }
            });
            ui.add_space(2.0);
            ui.small("Web 目录留空时点「启动」会自动探测；令牌留空会自动生成随机令牌并保存到凭据管理器，无需手工准备。");
            ui.small(format!("服务地址 {local_url}；点「启动」后会自动把下面的服务器地址对齐到这里。"));
            ui.small("访问令牌既是岛仔读取课表的凭据，也会作为 NOS_API_TOKEN 注入服务进程。");
            ui.small("默认只监听 127.0.0.1；改成 0.0.0.0 会把服务暴露到局域网，请先确认令牌与网络环境。");
            ui.small(format!("日志：{}", log_path.display()));
        });

        // ------------------------------------------------ 连接信息
        // 地址存设置文件，令牌只进凭据管理器。
        let mut url = self.settings.nos_base_url.clone();
        let mut token = String::new();
        let mut token_known = false;
        let _ = crate::nos::secrets::load().map(|value| {
            token_known = true;
            token = value;
        });

        settings_label(ui, "连接");
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("服务器地址").size(13.0));
                ui.text_edit_singleline(&mut url);
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("访问令牌").size(13.0));
                ui.text_edit_singleline(&mut token);
                if token_known && token.is_empty() {
                    ui.label(RichText::new("已保存（输入新值可替换）").size(11.5).color(egui::Color32::from_gray(150)));
                }
            });
            ui.add_space(4.0);
            ui.small("令牌由服务端环境变量 NOS_API_TOKEN 提供；服务器未配置时接口一律拒绝访问。");
            if self.settings.nos_server_enabled {
                ui.small("本地服务模式：令牌留空时点「启动」会自动生成并保存，地址也会自动对齐；要指定令牌才点「保存令牌」。服务还没起来时「保存并测试连接」报连接失败是正常的。");
            } else {
                ui.small("地址填到站点根即可，例如 http://192.168.1.10:8000 。");
            }
        });

        // 操作按钮：保存并测试 / 立即刷新 / 忘记令牌。
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                // 本地服务还没启动时，「测试连接」必然失败；给一个只落盘令牌的入口，
                // 让「保存令牌 → 回顶部点启动」成为一条不会误报失败的路径。
                if ui.button("保存令牌").clicked() {
                    let value = token.trim().to_string();
                    if value.is_empty() {
                        self.say("请先在「访问令牌」里填一个长随机串");
                    } else {
                        match crate::nos::secrets::save(&value) {
                            Ok(())=>self.say("访问令牌已保存到 Windows 凭据管理器，可以启动本地服务了"),
                            Err(error)=>self.say(format!("保存令牌失败：{error}")),
                        }
                    }
                }
                if ui.button("保存并测试连接").clicked() {
                    let token = token.trim().to_string();
                    if !token.is_empty() {
                        let _ = crate::nos::secrets::save(&token);
                    }
                    if !self.settings.nos_server_enabled {
                        self.settings.nos_base_url = url.trim().to_string();
                    }
                    self.settings.nos_enabled = true;
                    self.settings.save();
                    let target=if self.settings.nos_server_enabled {
                        self.nos_server_config().base_url()
                    } else {
                        url.trim().to_string()
                    };
                    pending.push(Cmd::NosTest(target, token));
                    pending.push(Cmd::NosReconfigure);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("立即刷新").clicked() {
                    pending.push(Cmd::NosRefresh);
                }
                if ui.button("忘记令牌").clicked() {
                    crate::nos::secrets::delete();
                }
            });
            ui.add_space(3.0);
            if let Some(error) = self.nos_error.clone() {
                ui.label(RichText::new(format!("连接失败：{error}")).size(11.5).color(egui::Color32::from_rgb(230,120,110)));
            } else if let Some(agenda) = self.nos_agenda.as_ref() {
                let courses = agenda.events.iter().filter(|e| e.is_course()).count();
                let semester = if self.nos_semester.is_empty() {
                    "未设置学期".to_string()
                } else {
                    self.nos_semester.clone()
                };
                ui.label(RichText::new(format!(
                    "已连接 · {semester} · 本次共 {courses} 节课"
                )).size(11.5));
            } else {
                ui.small("保存后会在后台自动同步，每 5 分钟刷新一次。");
            }
        });

        settings_label(ui, "显示");
        settings_card(ui, |ui| {
            setting_row(ui, "启用课表", "从「不止课表」读取课表并并入日程页", &mut self.settings.nos_enabled);
            ui.separator();
            setting_row(ui, "常驻栏显示下一节课", "收起时在岛上显示下一节课的开始时刻与课名", &mut self.settings.show_course);
            ui.add_space(3.0);
            ui.small("常驻栏只显示「下一节课」；完整课表在日程页里与其它日历一起显示。");
        });

        // 今日课程一览：让用户在设置里就能确认拉到的数据对不对。
        if let Some(agenda) = self.nos_agenda.as_ref() {
            let today = chrono::Local::now().date_naive();
            let key = today.format("%Y-%m-%d").to_string();
            let courses = agenda.today_courses(&key);
            settings_label(ui, "今日课程");
            settings_card(ui, |ui| {
                if courses.is_empty() {
                    ui.small("今天没有课。");
                } else {
                    for event in courses {
                        let clock = crate::nos::clock_label(&event.start_time);
                        let place = event.place();
                        let text = if place.is_empty() {
                            format!("{clock} {}", event.title)
                        } else {
                            format!("{clock} {} · {place}", event.title)
                        };
                        ui.label(RichText::new(text).size(12.5));
                    }
                }
            });
        }

        self.pending.extend(pending);
    }
}
