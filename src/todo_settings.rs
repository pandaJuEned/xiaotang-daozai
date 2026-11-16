//! 设置页「待办」页签：在设置窗口里直接查看、勾选、改正文、加一条与设置提醒。
//!
//! 岛内的待办页是像素级卡片绘制（一屏三行、滚轮翻页），搬不进 690×560 的设置窗口；
//! 这里用原生 egui 控件另做一份列表，两个入口读写同一份 `todo.txt`，从哪边改都一样。

use crate::app::App;
use crate::extras::{parse_due, settings_card, settings_hint, settings_label};
use crate::model::{self, TodoItem};
use crate::settings_theme::color as settings_color;
use eframe::egui::{self, Align, Color32, RichText};

/// 「提醒与重复」的编辑草稿：设置页用原生小窗口编辑，与岛内的卡片式编辑器互不影响。
pub(crate) struct ReminderDraft {
    pub(crate) index: usize,
    /// `YYYY-MM-DD HH:MM`，也可以只写 `HH:MM`（按今天算）；空串表示不提醒。
    pub(crate) due: String,
    pub(crate) repeat: u8,
    pub(crate) error: String,
    pub(crate) focused: bool,
}

impl ReminderDraft {
    fn new(index: usize, item: &TodoItem) -> Self {
        Self {
            index,
            due: item.due.clone().unwrap_or_default(),
            repeat: item.repeat,
            error: String::new(),
            focused: false,
        }
    }
}

/// 一行正文的编辑草稿 id。按行号存，删除或新增后由 [`clear_drafts`] 统一清掉。
fn draft_id(index: usize) -> egui::Id {
    egui::Id::new(("settings-todo-text", index))
}

/// 丢弃所有正文草稿。行号整体错位后再按行号取草稿就会串到别人身上，直接清空最稳。
fn clear_drafts(ctx: &egui::Context, count: usize) {
    for index in 0..count.max(1) {
        ctx.data_mut(|data| data.remove::<String>(draft_id(index)));
    }
}

impl App {
    // ---------------------------------------------------------------
    // 数据操作：界面只负责把输入交进来，逻辑单独放，方便直接写单测。
    // ---------------------------------------------------------------
    /// 新增一条待办。插到最前面，与岛内待办页的「加一个」保持一致。
    pub(crate) fn todo_add(&mut self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        self.todos.items.insert(0, TodoItem::new(text));
        self.todos.save();
        // 岛内待办页也回到顶部，新条目就在第一行。
        self.todo_first = 0;
        true
    }

    /// 就地改正文。空内容视为放弃修改，保留原文。
    pub(crate) fn todo_rename(&mut self, index: usize, text: &str) -> bool {
        let text = text.trim();
        let Some(item) = self.todos.items.get_mut(index) else {
            return false;
        };
        if text.is_empty() || item.text == text {
            return false;
        }
        item.text = text.to_string();
        self.todos.save();
        true
    }

    /// 勾选 / 取消勾选（重复待办记的是「今天已完成」）。
    pub(crate) fn todo_toggle(&mut self, index: usize) -> bool {
        let today = self.today.clone();
        let Some(item) = self.todos.items.get_mut(index) else {
            return false;
        };
        item.toggle(&today);
        self.todos.save();
        true
    }

    pub(crate) fn todo_remove(&mut self, index: usize) -> bool {
        if index >= self.todos.items.len() {
            return false;
        }
        self.todos.items.remove(index);
        self.todos.save();
        true
    }

    /// 保存提醒与重复。`due` 为空表示取消提醒。
    pub(crate) fn todo_save_reminder(&mut self, index: usize, due: &str, repeat: u8) -> Result<(), String> {
        let today = self.today.clone();
        let value = if due.trim().is_empty() {
            None
        } else {
            // `parse_due` 既收 `18:30`（按今天算）也收完整的 `2026-11-20 09:30`。
            Some(parse_due(due, &today).ok_or_else(|| "时间格式应为 18:30 或 2026-11-20 09:30".to_string())?)
        };
        let repeat = repeat & model::REPEAT_ALL;
        let Some(item) = self.todos.items.get_mut(index) else {
            return Err("这条待办已经不在清单里了".into());
        };
        if item.repeat != repeat {
            // 换了重复规则，旧规则下的完成记录不再成立。
            item.done_on = None;
        }
        item.due = value;
        item.repeat = repeat;
        item.reminded = false;
        item.reminded_on = None;
        self.todos.save();
        Ok(())
    }

    // ---------------------------------------------------------------
    // 界面
    // ---------------------------------------------------------------
    /// 「待办」页签正文。
    pub(crate) fn todo_settings(&mut self, ui: &mut egui::Ui) {
        let today = self.today.clone();
        let (done, total) = self.todos.progress(&today);
        let pending = total.saturating_sub(done);
        let dim = settings_color(ui.ctx(), Color32::from_gray(160), Color32::from_gray(105));
        let ink = settings_color(ui.ctx(), Color32::WHITE, Color32::from_rgb(30, 34, 41));

        settings_label(ui, "今日进度");
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                let color = if total > 0 && done == total { Color32::from_rgb(48, 184, 92) } else { ink };
                ui.label(RichText::new(done.to_string()).size(24.0).strong().color(color));
                ui.label(RichText::new(format!("/ {total}")).size(13.0).color(dim));
                ui.add_space(10.0);
                let line = if total == 0 {
                    "清单还是空的，在下面加第一条".to_string()
                } else if pending == 0 {
                    "今天的都做完了".to_string()
                } else {
                    format!("今天还有 {pending} 项")
                };
                ui.label(RichText::new(line).size(12.5).color(dim));
            });
        });

        settings_label(ui, "待办清单");
        settings_card(ui, |ui| {
            self.todo_add_row(ui);
            let order = self.todos.display_order(&today);
            // 行宽只量一次：每行按同一个宽度画，右侧按钮才会对齐。
            let row_w = ui.available_width();
            if order.is_empty() {
                ui.label(RichText::new("还没有待办。上面输入内容后回车即可添加。").size(12.5).color(dim));
            } else {
                let mut remove = None;
                for &index in &order {
                    ui.add_space(1.0);
                    if self.todo_row(ui, index, row_w) {
                        remove = Some(index);
                        break;
                    }
                }
                if let Some(index) = remove {
                    self.todo_remove(index);
                    // 行号整体前移，草稿按行号存会对不上，直接清空。
                    clear_drafts(ui.ctx(), self.todos.items.len() + 1);
                }
            }
            ui.add_space(2.0);
            settings_hint(ui, "勾选即完成；正文改完按回车或点别处保存；右侧「提醒」可设时间与每周重复。");
        });

        // NOS 待办：与岛内待办页同一口径，只在设置页展示，不写回本机清单。
        let nos = self.nos_todos.clone();
        if !nos.is_empty() {
            settings_label(ui, "来自不止课表");
            settings_card(ui, |ui| {
                for item in &nos {
                    nos_row(ui, item, &today, dim);
                }
                settings_hint(ui, "这些待办在「不止课表」里维护，岛仔只读展示。");
            });
        }

        settings_label(ui, "岛内入口");
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add_sized([112.0, 30.0], egui::Button::new("打开待办页")).clicked() {
                    self.settings_open = false;
                    self.show_page(4);
                }
                ui.label(RichText::new("岛内待办页是紧凑卡片视图，适合日常勾选。").size(12.0).color(dim));
            });
        });
    }

    /// 新增行：输入框 + 「添加」，回车也能提交。
    fn todo_add_row(&mut self, ui: &mut egui::Ui) {
        let buffer_id = egui::Id::new("settings-todo-add");
        let field_id = egui::Id::new("settings-todo-add-field");
        let mut buffer = ui.data(|data| data.get_temp::<String>(buffer_id)).unwrap_or_default();
        let mut submit = false;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 84.0).max(90.0);
            let edit = ui.add_sized(
                [width, 28.0],
                egui::TextEdit::singleline(&mut buffer)
                    .id(field_id)
                    .vertical_align(Align::Center)
                    .hint_text("加一条待办，回车确认"),
            );
            // 单行输入框按回车会自己失焦，所以「失焦 + 这一帧按了回车」就是提交。
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            if ui.add_sized([72.0, 28.0], egui::Button::new("添加")).clicked() {
                submit = true;
            }
        });
        if submit && self.todo_add(&buffer) {
            buffer.clear();
            // 新条目插在最前面，行号整体后移，正文草稿按行号存会对不上，清掉。
            clear_drafts(ui.ctx(), self.todos.items.len() + 1);
            // 连续录入时不抢焦点，直接接着打第二条。
            ui.memory_mut(|memory| memory.request_focus(field_id));
        }
        ui.data_mut(|data| data.insert_temp(buffer_id, buffer));
    }

    /// 一行本机待办：勾选、改正文、提醒、删除。返回 true 表示这一行点了删除。
    ///
    /// `row_w` 由调用方算好一次：每行都用同一个宽度，右侧「提醒」和「×」才能排成一条竖线。
    /// （跟着行内游标走的话，子布局的溢出会让每一行比上一行再往右挪一点。）
    fn todo_row(&mut self, ui: &mut egui::Ui, index: usize, row_w: f32) -> bool {
        let today = self.today.clone();
        let Some(item) = self.todos.items.get(index) else {
            return false;
        };
        let text = item.text.clone();
        let done = item.done_for(&today);
        let scheduled = item.scheduled_on(&today);
        let repeat = item.repeat;
        let mut tag = model::repeat_tag(item.repeat);
        if let Some(due) = item.due.as_deref() {
            let clock = due.get(11..16).unwrap_or(due);
            tag = if tag.is_empty() { clock.to_string() } else { format!("{tag} {clock}") };
        }
        let reminder_hint = item
            .reminder_text()
            .map(|due| format!("提醒：{due} · 点击修改"))
            .unwrap_or_else(|| "设置提醒与每周重复".to_string());

        let dim = settings_color(ui.ctx(), Color32::from_gray(150), Color32::from_gray(120));
        let ink = settings_color(ui.ctx(), Color32::WHITE, Color32::from_rgb(30, 34, 41));
        let text_color = if done || !scheduled { dim } else { ink };
        let check_hint = if !scheduled {
            format!("{}，今天不用做；勾选只记录今天", model::repeat_label(repeat))
        } else if done {
            "标记为未完成".to_string()
        } else {
            "标记为完成".to_string()
        };

        let id = draft_id(index);
        let mut draft = ui.data(|data| data.get_temp::<String>(id)).unwrap_or_else(|| text.clone());
        let mut remove = false;
        let mut remind = false;
        ui.allocate_ui_with_layout(
            egui::vec2(row_w, 26.0),
            egui::Layout::left_to_right(Align::Center),
            |ui| {
                let mut checked = done;
                if ui.checkbox(&mut checked, "").on_hover_text(check_hint).changed() {
                    self.todo_toggle(index);
                }
                // 右侧固定预留一条尾巴，正文不会顶到按钮上。
                let width = (ui.available_width() - 176.0).max(90.0);
                let edit = ui.add_sized(
                    [width, 24.0],
                    egui::TextEdit::singleline(&mut draft)
                        .id(egui::Id::new(("settings-todo-edit", index)))
                        .frame(false)
                        .vertical_align(Align::Center)
                        .text_color(text_color)
                        .hint_text("（空）"),
                );
                // 草稿存在 egui 的临时数据里：敲字时不写盘，失焦或回车才一次性提交。
                if edit.lost_focus() {
                    self.todo_rename(index, &draft);
                    ui.data_mut(|data| data.remove::<String>(id));
                } else if edit.changed() {
                    ui.data_mut(|data| data.insert_temp(id, draft.clone()));
                }
                // 从右往左排：× 钉在行右缘，「提醒」紧挨着它，标签长短都不会错位。
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("×").size(15.0).color(dim)).frame(false).min_size(egui::vec2(22.0, 22.0)))
                        .on_hover_text("删除这条待办")
                        .clicked()
                    {
                        remove = true;
                    }
                    let label = if tag.is_empty() { "提醒".to_string() } else { tag.clone() };
                    let label_color = if tag.is_empty() { dim } else { Color32::from_rgb(86, 156, 236) };
                    if ui
                        .add(egui::Button::new(RichText::new(label).size(11.0).color(label_color)).frame(false))
                        .on_hover_text(reminder_hint)
                        .clicked()
                    {
                        remind = true;
                    }
                });
            },
        );
        if remind {
            if let Some(item) = self.todos.items.get(index) {
                self.todo_reminder = Some(ReminderDraft::new(index, item));
            }
        }
        remove
    }

    /// 「提醒与重复」原生小窗口。条目不在了或窗口被关掉时自动收起。
    pub(crate) fn todo_reminder_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.todo_reminder.take() else {
            return;
        };
        let Some(title) = self.todos.items.get(draft.index).map(|item| item.text.clone()) else {
            return;
        };
        let dim = settings_color(ctx, Color32::from_gray(160), Color32::from_gray(105));
        // 没选中的星期也要看得出是按钮：只留文字在深色底上太淡了。
        let chip_off = settings_color(ctx, Color32::from_rgb(56, 56, 60), Color32::from_rgb(226, 230, 236));
        let chip_off_text = settings_color(ctx, Color32::from_gray(225), Color32::from_gray(50));
        let mut open = true;
        let mut save = false;
        let mut close = false;
        egui::Window::new("提醒与重复")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(300.0);
                ui.label(RichText::new(title.as_str()).size(13.0).strong());
                ui.add_space(8.0);
                ui.label(RichText::new("提醒时间").size(11.5).color(dim));
                let edit = ui.add_sized(
                    [ui.available_width(), 28.0],
                    egui::TextEdit::singleline(&mut draft.due)
                        .vertical_align(Align::Center)
                        .hint_text("18:30，或 2026-11-20 09:30"),
                );
                if !draft.focused {
                    edit.request_focus();
                    draft.focused = true;
                }
                ui.add_space(8.0);
                ui.label(RichText::new("重复").size(11.5).color(dim));
                ui.horizontal(|ui| {
                    for (day, name) in model::WEEKDAY_NAMES.iter().enumerate() {
                        let on = draft.repeat & (1 << day) != 0;
                        let text = RichText::new(*name).size(12.0).color(if on { Color32::WHITE } else { chip_off_text });
                        let button = egui::Button::new(text)
                            .fill(if on { Color32::from_rgb(45, 105, 165) } else { chip_off })
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(6.0)
                            .min_size(egui::vec2(32.0, 24.0));
                        if ui.add(button).clicked() {
                            draft.repeat ^= 1 << day;
                            draft.error.clear();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.small_button("不重复").clicked() {
                        draft.repeat = 0;
                        draft.error.clear();
                    }
                    if ui.small_button("每天").clicked() {
                        draft.repeat = model::REPEAT_ALL;
                        draft.error.clear();
                    }
                    if ui.small_button("工作日").clicked() {
                        draft.repeat = model::REPEAT_WORKDAYS;
                        draft.error.clear();
                    }
                });
                ui.add_space(6.0);
                if draft.error.is_empty() {
                    let hint = if draft.repeat == 0 {
                        "只提醒一次，勾选完成后自动失效".to_string()
                    } else {
                        format!("{} · 到点提醒", model::repeat_label(draft.repeat))
                    };
                    ui.label(RichText::new(hint).size(11.0).color(dim));
                } else {
                    ui.label(RichText::new(draft.error.as_str()).size(11.5).color(Color32::from_rgb(228, 92, 82)));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("取消提醒").clicked() {
                        draft.due.clear();
                        draft.error.clear();
                        draft.repeat = 0;
                        save = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("保存").clicked() {
                            save = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if save {
            match self.todo_save_reminder(draft.index, &draft.due, draft.repeat) {
                Ok(()) => {
                    close = true;
                    self.say("提醒已保存");
                }
                Err(error) => draft.error = error,
            }
        }
        if close || !open {
            self.todo_reminder = None;
        } else {
            self.todo_reminder = Some(draft);
        }
    }
}

/// NOS 待办只读行：状态点 + 标题 + 截止时刻 + 来源标签。
fn nos_row(ui: &mut egui::Ui, item: &TodoItem, today: &str, dim: Color32) {
    let done = item.done_for(today);
    let due = item.due.as_deref().unwrap_or_default();
    let clock = due.get(11..16).unwrap_or("");
    let blue = Color32::from_rgb(86, 156, 236);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 18.0), egui::Sense::hover());
        let center = rect.center();
        if done {
            ui.painter().circle(center, 4.0, Color32::from_rgb(48, 184, 92), egui::Stroke::NONE);
        } else {
            ui.painter().circle_stroke(center, 4.0, egui::Stroke::new(1.3_f32, blue));
        }
        let color = if done { dim } else { settings_color(ui.ctx(), Color32::from_gray(230), Color32::from_rgb(40, 44, 52)) };
        ui.label(RichText::new(item.text.as_str()).size(12.5).color(color));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("NOS").size(10.5).color(blue));
            if !clock.is_empty() {
                ui.label(RichText::new(format!("截止 {clock}")).size(10.5).color(dim));
            }
        });
    });
}
