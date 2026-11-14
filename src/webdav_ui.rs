//! 设置 → 音乐与歌词 里的「WebDAV 音乐」：连接配置、目录浏览与播放条。
//!
//! 网络请求与解码都在 `resources::WEBDAV` 泳道的线程里，界面只读快照；
//! 密码只在「测试连接」「播放」这些动作上写一次凭据管理器，不随输入轮询写盘。

use std::sync::mpsc;

use eframe::egui::{self, Color32, RichText};

use crate::app::App;
use crate::extras::{settings_card, settings_control_row, settings_hint, settings_label};
use crate::webdav::{self, Entry};

enum Msg {
    Listed(Result<(String, Vec<Entry>), String>),
    Tested(Result<usize, String>),
}

pub struct Ui {
    pub url: String,
    pub user: String,
    pub password: String,
    /// 是否已经从凭据管理器读过密码（打开设置时才读）。
    pub password_loaded: bool,
    pub show_password: bool,
    /// 最近一次操作的结果：(是否成功, 文案)。
    pub note: Option<(bool, String)>,
    pub busy: bool,
    pub collection: String,
    pub entries: Vec<Entry>,
    pub loading: bool,
    pub error: Option<String>,
    /// 当前目录是否已经取过一次：用来区分「还没读」和「读到了空目录」。
    pub loaded: bool,
    /// 拖动进度条期间的临时值；松手才真正跳转。
    pub draft: Option<f64>,
    tx: Option<mpsc::Sender<Msg>>,
    rx: Option<mpsc::Receiver<Msg>>,
    #[cfg(windows)]
    pub engine: Option<crate::webdav_player::Engine>,
}

impl Default for Ui {
    fn default() -> Self {
        let cfg = webdav::Config::load();
        // 密码不在这里读：App::new 会被测试反复调用，凭据管理器只该在真正打开设置时碰。
        Self {
            url: cfg.url,
            user: cfg.user,
            password: String::new(),
            password_loaded: false,
            show_password: false,
            note: None,
            busy: false,
            collection: cfg.dir.clone(),
            entries: Vec::new(),
            loading: false,
            error: None,
            loaded: false,
            draft: None,
            tx: None,
            rx: None,
            #[cfg(windows)]
            engine: None,
        }
    }
}

impl Ui {
    pub fn config(&self) -> webdav::Config {
        webdav::Config {
            url: self.url.trim().to_string(),
            user: self.user.trim().to_string(),
            dir: self.collection.clone(),
        }
    }

    /// 收取后台线程的结果。先把消息取干净再改状态，避免与 `rx` 的借用冲突。
    fn receive(&mut self) {
        let mut messages = Vec::new();
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.rx = None;
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                Msg::Listed(result) => {
                    self.loading = false;
                    self.busy = false;
                    match result {
                        Ok((collection, entries)) => {
                            self.collection = collection;
                            self.entries = entries;
                            self.error = None;
                            self.loaded = true;
                        }
                        Err(error) => {
                            self.error = Some(error.clone());
                            self.note = Some((false, error));
                        }
                    }
                }
                Msg::Tested(result) => {
                    self.busy = false;
                    match result {
                        Ok(count) => self.note = Some((true, format!("连接成功，当前目录有 {count} 个条目"))),
                        Err(error) => self.note = Some((false, error)),
                    }
                }
            }
        }
    }

    fn spawn(&mut self, work: impl FnOnce(mpsc::Sender<Msg>) + Send + 'static) {
        if self.rx.is_none() || self.tx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        let Some(tx) = self.tx.clone() else { return };
        let spawned = crate::resources::WEBDAV
            .acquire()
            .map(|lease| lease.spawn(move || work(tx)))
            .flatten();
        if spawned.is_none() {
            self.note = Some((false, "后台任务已满，请稍后再试".into()));
        }
    }
}

impl App {
    /// 设置窗口 tab 1 的 WebDAV 区块。
    pub(crate) fn webdav_settings(&mut self, ui: &mut egui::Ui) {
        self.webdav.receive();
        if !self.webdav.password_loaded {
            self.webdav.password_loaded = true;
            let cfg = self.webdav.config();
            if cfg.ready() {
                self.webdav.password = webdav::load_password(&cfg).unwrap_or_default();
            }
        }
        let ctx = ui.ctx().clone();

        settings_label(ui, "WebDAV 音乐");
        settings_card(ui, |ui| {
            settings_control_row(ui, "服务器", 56.0, |ui| {
                ui.add_sized(
                    [ui.available_width(), 30.0],
                    egui::TextEdit::singleline(&mut self.webdav.url)
                        .vertical_align(egui::Align::Center)
                        .hint_text("https://nas.example.com/music"),
                )
            });
            settings_control_row(ui, "用户名", 56.0, |ui| {
                ui.add_sized(
                    [ui.available_width(), 30.0],
                    egui::TextEdit::singleline(&mut self.webdav.user)
                        .vertical_align(egui::Align::Center)
                        .hint_text("WebDAV 账号"),
                )
            });
            settings_control_row(ui, "密码", 56.0, |ui| {
                let field = ui.add_sized(
                    [(ui.available_width() - 48.0).max(80.0), 30.0],
                    egui::TextEdit::singleline(&mut self.webdav.password)
                        .vertical_align(egui::Align::Center)
                        .password(!self.webdav.show_password)
                        .hint_text("留空表示匿名只读"),
                );
                if ui
                    .add_sized([48.0, 30.0], egui::Button::new(if self.webdav.show_password { "隐藏" } else { "显示" }))
                    .clicked()
                {
                    self.webdav.show_password = !self.webdav.show_password;
                }
                field
            });
            ui.horizontal(|ui| {
                let idle = !self.webdav.busy;
                if ui
                    .add_enabled(idle, egui::Button::new(if self.webdav.busy { "连接中…" } else { "测试连接" }))
                    .clicked()
                {
                    self.start_webdav_test(&ctx);
                }
                if ui.button("重新读取目录").clicked() {
                    let target = self.webdav.collection.clone();
                    self.open_webdav_dir(&ctx, target);
                }
            });
            if let Some((ok, text)) = &self.webdav.note {
                ui.add_space(3.0);
                let color = if *ok { Color32::from_rgb(46, 160, 88) } else { Color32::from_rgb(196, 84, 62) };
                ui.add(egui::Label::new(RichText::new(text).size(12.0).color(color)).wrap());
            }
            settings_hint(
                ui,
                "密码保存在 Windows 凭据管理器，配置文件只记地址、账号与当前目录。只支持 https，本机回环可用 http。",
            );
        });

        self.webdav_browser(ui, &ctx);
        self.webdav_transport(ui);

        // 地址或账号改了就直接落盘，不用额外点保存。
        let cfg = self.webdav.config();
        if cfg != self.webdav_saved && cfg.save().is_ok() {
            self.webdav_saved = cfg;
        }
    }

    fn start_webdav_test(&mut self, ctx: &egui::Context) {
        let cfg = self.webdav.config();
        let password = self.webdav.password.clone();
        let Some(base) = cfg.base() else {
            self.webdav.note = Some((false, "服务器地址无效，需要 https:// 开头".into()));
            return;
        };
        if cfg.user.is_empty() {
            self.webdav.note = Some((false, "请填写用户名（匿名服务器可填任意值）".into()));
            return;
        }
        let _ = cfg.save();
        if !password.is_empty() {
            if let Err(error) = webdav::save_password(&cfg, &password) {
                self.webdav.note = Some((false, error));
                return;
            }
        }
        self.webdav_saved = cfg.clone();
        self.webdav.busy = true;
        self.webdav.note = None;
        let paint = ctx.clone();
        let user = cfg.user.clone();
        let probe = base.clone();
        self.webdav.spawn(move |tx| {
            let result = webdav::list(&probe, &user, &password).map(|entries| entries.len());
            let _ = tx.send(Msg::Tested(result));
            paint.request_repaint();
        });
        // 顺手把目录读出来，连接成功就不用再点一次。
        self.open_webdav_dir(ctx, base);
    }

    fn open_webdav_dir(&mut self, ctx: &egui::Context, collection: String) {
        let cfg = self.webdav.config();
        let Some(base) = cfg.base() else {
            self.webdav.error = Some("服务器地址无效".into());
            return;
        };
        let target = if collection.trim().is_empty() { base } else { collection };
        let password = self.webdav.password.clone();
        let user = cfg.user.clone();
        self.webdav.loading = true;
        self.webdav.error = None;
        let paint = ctx.clone();
        self.webdav.spawn(move |tx| {
            let result = webdav::list(&target, &user, &password).map(|entries| (target, entries));
            let _ = tx.send(Msg::Listed(result));
            paint.request_repaint();
        });
    }

    fn webdav_browser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // 还没取过目录时不占地方：点过「测试连接」或「重新读取目录」才展开。
        if !self.webdav.loaded && self.webdav.error.is_none() {
            return;
        }
        settings_label(ui, "曲库");
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("上一级").clicked() {
                    let current = self.webdav.collection.clone();
                    if let Some(parent) = parent_collection(&current) {
                        self.open_webdav_dir(ctx, parent);
                    }
                }
                let path = self.webdav.collection.clone();
                ui.add(
                    egui::Label::new(RichText::new(short_path(&path)).size(11.5).color(ui.visuals().weak_text_color()))
                        .wrap(),
                );
            });
            ui.separator();
            if self.webdav.loading {
                ui.label("正在读取目录…");
                return;
            }
            if let Some(error) = &self.webdav.error {
                ui.add(egui::Label::new(RichText::new(error).size(12.0).color(Color32::from_rgb(196, 84, 62))).wrap());
                return;
            }
            if self.webdav.entries.is_empty() {
                ui.label(RichText::new("这个目录里没有条目").size(12.0));
                return;
            }
            let entries = self.webdav.entries.clone();
            egui::ScrollArea::vertical().max_height(210.0).id_salt("webdav-list").show(ui, |ui| {
                for entry in entries.iter() {
                    let label = if entry.is_dir {
                        format!("📁 {}", entry.name)
                    } else if entry.playable() {
                        format!("♪ {}   {}", entry.name, size_text(entry.bytes))
                    } else {
                        format!("· {}", entry.name)
                    };
                    let response = ui.add_enabled(
                        entry.is_dir || entry.playable(),
                        egui::Button::new(RichText::new(label).size(12.5)).frame(false),
                    );
                    if response.clicked() {
                        if entry.is_dir {
                            self.open_webdav_dir(ctx, entry.url.clone());
                        } else {
                            self.play_webdav(entry, ctx);
                        }
                    }
                }
            });
        });
    }

    #[cfg(windows)]
    fn play_webdav(&mut self, entry: &Entry, ctx: &egui::Context) {
        let playlist: Vec<crate::webdav_player::Track> = self
            .webdav
            .entries
            .iter()
            .filter(|item| item.playable())
            .map(|item| crate::webdav_player::Track { name: item.name.clone(), url: item.url.clone() })
            .collect();
        let Some(position) = playlist.iter().position(|track| track.url == entry.url) else { return };
        if self.webdav.engine.is_none() {
            let cfg = self.webdav.config();
            let _ = cfg.save();
            match crate::webdav_player::Engine::start(ctx.clone(), cfg, self.webdav.password.clone()) {
                Some(engine) => self.webdav.engine = Some(engine),
                None => {
                    self.webdav.note = Some((false, "无法启动播放线程".into()));
                    return;
                }
            }
        }
        if let Some(engine) = &self.webdav.engine {
            engine.play(playlist, position);
        }
    }

    #[cfg(not(windows))]
    fn play_webdav(&mut self, _entry: &Entry, _ctx: &egui::Context) {}

    fn webdav_transport(&mut self, ui: &mut egui::Ui) {
        #[cfg(not(windows))]
        {
            let _ = ui;
        }
        #[cfg(windows)]
        {
            let Some(engine) = &self.webdav.engine else { return };
            let state = engine.snapshot();
            let Some(track) = state.current().cloned() else { return };
            settings_label(ui, "正在播放");
            settings_card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(&track.name).size(12.5).strong()).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("下一首").clicked() {
                            engine.next();
                        }
                        if ui.button(if state.playing() { "暂停" } else { "播放" }).clicked() {
                            engine.toggle();
                        }
                        if ui.button("上一首").clicked() {
                            engine.prev();
                        }
                    });
                });
                match &state.status {
                    crate::webdav_player::Status::Downloading(progress) => {
                        ui.add(egui::ProgressBar::new(*progress).text(format!("下载中 {:.0}%", progress * 100.0)));
                    }
                    crate::webdav_player::Status::Error(error) => {
                        let error = error.clone();
                        ui.add(egui::Label::new(RichText::new(error).size(12.0).color(Color32::from_rgb(196, 84, 62))).wrap());
                    }
                    _ => {}
                }
                let duration = state.duration.max(0.1);
                let mut position = self.webdav.draft.unwrap_or(state.position).clamp(0.0, duration);
                let response = ui.add(egui::Slider::new(&mut position, 0.0..=duration).show_value(false));
                if response.dragged() {
                    self.webdav.draft = Some(position);
                }
                if response.drag_stopped() {
                    let target = self.webdav.draft.take().unwrap_or(position);
                    engine.seek(target);
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} / {}", mmss(state.position), mmss(state.duration))).size(11.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut volume = state.volume;
                        if ui.add(egui::Slider::new(&mut volume, 0..=100).show_value(false)).changed() {
                            engine.volume(volume);
                        }
                        ui.label(RichText::new("音量").size(11.0));
                    });
                });
            });
        }
    }
}

fn mmss(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn size_text(bytes: u64) -> String {
    if bytes == 0 {
        String::new()
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

/// 目录的上一级；已经在根上时返回 `None`。
fn parent_collection(current: &str) -> Option<String> {
    let mut url = url::Url::parse(current).ok()?;
    let path = url.path().trim_end_matches('/');
    // 根目录的 path 是空串，直接没有上一级。
    let (head, _) = path.rsplit_once('/')?;
    let parent = if head.is_empty() { "/".to_string() } else { format!("{head}/") };
    url.set_path(&parent);
    Some(url.to_string())
}

/// 路径太长时只显示主机与最后两段。
fn short_path(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else { return url.to_string() };
    let segments: Vec<_> = parsed.path().split('/').filter(|part| !part.is_empty()).collect();
    let tail = segments.iter().rev().take(2).rev().cloned().collect::<Vec<_>>().join("/");
    format!("{} / {}", parsed.host_str().unwrap_or(""), tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_collection_walks_up_and_stops_at_root() {
        assert_eq!(
            parent_collection("https://nas.local/music/rock/"),
            Some("https://nas.local/music/".into())
        );
        assert_eq!(parent_collection("https://nas.local/music/"), Some("https://nas.local/".into()));
        assert_eq!(parent_collection("https://nas.local/"), None);
    }

    #[test]
    fn short_path_keeps_host_and_last_two_segments() {
        assert_eq!(short_path("https://nas.local/a/b/c/"), "nas.local / b/c");
        assert_eq!(short_path("https://nas.local/"), "nas.local / ");
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(size_text(0), "");
        assert_eq!(size_text(2048), "2 KB");
        assert_eq!(size_text(5 * 1_048_576), "5.0 MB");
    }
}
