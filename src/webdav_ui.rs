//! 设置 → 音乐与歌词 里的「WebDAV 音乐」：连接配置、目录浏览与播放条。
//!
//! 网络请求与解码都在 `resources::WEBDAV` 泳道的线程里，界面只读快照；
//! 密码只在「测试连接」「播放」这些动作上写一次凭据管理器，不随输入轮询写盘。

use std::sync::mpsc;

use eframe::egui::{self, Color32, RichText};

use crate::app::{App, Cmd};
use crate::extras::{settings_card, settings_control_row, settings_hint, settings_label};
use crate::settings_theme::color as settings_color;
use crate::webdav::{self, Entry};

enum Msg {
    Listed(Result<(String, Vec<Entry>), String>),
    Tested(Result<usize, String>),
    /// 从 NOS 拉取连接配置的结果（提示文案）。
    Pulled(Result<String, String>),
}

pub struct Ui {
    pub url: String,
    pub user: String,
    pub password: String,
    /// 连接配置由 NOS 集中下发：三项只读，改到 NOS Web「我的」页。
    pub from_nos: bool,
    pub from_nos_at: String,
    /// 是否已经从凭据管理器读过密码（打开设置时才读）。
    pub password_loaded: bool,
    pub show_password: bool,
    /// 最近一次操作的结果：(是否成功, 文案)。
    pub note: Option<(bool, String)>,
    pub busy: bool,
    pub collection: String,
    pub entries: Vec<Entry>,
    /// 播放序列：按加入顺序播放，不与目录里的其它曲目混在一起。
    pub queue: Vec<Entry>,
    /// 乱序播放：切歌时随机选一首，播完自动续播也遵循。
    pub shuffle: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// 当前目录是否已经取过一次：用来区分「还没读」和「读到了空目录」。
    pub loaded: bool,
    /// 岛上自动拉过一次目录，失败后不再每帧重试。
    pub auto_tried: bool,
    /// 拖动进度条期间的临时值；松手才真正跳转。
    pub draft: Option<f64>,
    /// 拖动音量条期间的临时值；松手后回到播放器的实际音量。
    pub volume_draft: Option<u8>,
    tx: Option<mpsc::Sender<Msg>>,
    rx: Option<mpsc::Receiver<Msg>>,
    #[cfg(windows)]
    pub engine: Option<crate::webdav_player::Engine>,
}

impl Default for Ui {
    fn default() -> Self {
        let cfg = webdav::Config::load();
        // 播放序列与顺序跨重启保留：音乐页不依赖设置页先打开过。
        let saved_queue = webdav::QueueFile::load();
        // 上一份目录也缓存下来：重启后岛上立刻能显示歌单，联网成功再覆盖。
        let cached_dir = webdav::DirCache::load();
        // 密码不在这里读：App::new 会被测试反复调用，凭据管理器只该在真正打开设置时碰。
        Self {
            url: cfg.url,
            user: cfg.user,
            password: String::new(),
            from_nos: cfg.from_nos,
            from_nos_at: cfg.from_nos_at,
            password_loaded: false,
            show_password: false,
            note: None,
            busy: false,
            collection: if cached_dir.collection.is_empty() { cfg.dir.clone() } else { cached_dir.collection },
            entries: cached_dir.entries,
            queue: saved_queue.tracks,
            shuffle: saved_queue.shuffle,
            loading: false,
            error: None,
            loaded: false,
            auto_tried: false,
            draft: None,
            volume_draft: None,
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
            from_nos: self.from_nos,
            from_nos_at: self.from_nos_at.clone(),
        }
    }

    /// 岛上播放列表：有自定义序列就用序列，否则用当前曲库目录里的音频。
    /// 这样「曲库里有歌」和「列表有歌」是同一件事，不需要先逐首点「＋序列」。
    pub fn playlist(&self) -> Vec<Entry> {
        if !self.queue.is_empty() {
            self.queue.clone()
        } else {
            self.entries.iter().filter(|item| item.playable()).cloned().collect()
        }
    }

    /// 队列或播放顺序变了就落盘，重启后音乐页仍有列表。
    pub fn persist_queue(&mut self) {
        webdav::QueueFile {
            tracks: self.queue.clone(),
            shuffle: self.shuffle,
        }.save();
    }

    /// 收取后台线程的结果。先把消息取干净再改状态，避免与 `rx` 的借用冲突。
    pub fn receive(&mut self) {
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
                            // 存一份目录，下次启动岛上不用等网络就有歌单。
                            webdav::DirCache {
                                collection: self.collection.clone(),
                                entries: self.entries.clone(),
                            }.save();
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
                Msg::Pulled(result) => {
                    self.busy = false;
                    match result {
                        Ok(message) => {
                            // 配置可能已变：重新读入界面字段与凭据，避免显示旧的。
                            let cfg = webdav::Config::load();
                            self.url = cfg.url.clone();
                            self.user = cfg.user.clone();
                            self.from_nos = cfg.from_nos;
                            self.from_nos_at = cfg.from_nos_at.clone();
                            self.password = webdav::load_password(&cfg).unwrap_or_default();
                            self.password_loaded = true;
                            // 同源目录保留，换服务器则由 apply 清空：直接读回磁盘值。
                            self.collection = cfg.dir.clone();
                            self.entries.clear();
                            self.loaded = false;
                            self.note = Some((true, message));
                        }
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

        let managed = self.webdav.from_nos;
        settings_label(ui, "WebDAV 音乐");
        settings_card(ui, |ui| {
            if managed {
                // 连接配置由 NOS 集中下发：只读展示，避免各端各填一份后互相漂移。
                settings_hint(ui, "连接配置由「不止课表」统一管理：请到 NOS Web「我的」页填写地址、账号与密码，各端连接后自动获取。");
                ui.add_space(3.0);
                settings_control_row(ui, "服务器", 56.0, |ui| {
                    ui.add_sized(
                        [ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut self.webdav.url)
                            .vertical_align(egui::Align::Center)
                            .interactive(false),
                    )
                });
                settings_control_row(ui, "用户名", 56.0, |ui| {
                    ui.add_sized(
                        [ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut self.webdav.user)
                            .vertical_align(egui::Align::Center)
                            .interactive(false),
                    )
                });
            } else {
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
            }
            ui.horizontal(|ui| {
                let idle = !self.webdav.busy;
                if !managed && ui
                    .add_enabled(idle, egui::Button::new(if self.webdav.busy { "连接中…" } else { "测试连接" }))
                    .clicked()
                {
                    self.start_webdav_test(&ctx);
                }
                if ui.button("重新读取目录").clicked() {
                    let target = self.webdav.collection.clone();
                    self.open_webdav_dir(&ctx, target);
                }
                // 未托管时提供「从 NOS 获取」；已托管时提供「重新获取」。
                if self.nos_config().ready() && ui.button(if managed { "从 NOS 重新获取" } else { "从 NOS 获取配置" }).clicked() {
                    self.pull_webdav_from_nos(&ctx);
                }
            });
            if let Some((ok, text)) = &self.webdav.note {
                ui.add_space(3.0);
                let color = if *ok { Color32::from_rgb(46, 160, 88) } else { Color32::from_rgb(196, 84, 62) };
                ui.add(egui::Label::new(RichText::new(text).size(12.0).color(color)).wrap());
            }
            settings_hint(
                ui,
                if managed {
                    "密码由 NOS 下发后存进本机凭据管理器。改连接请到 NOS Web「我的」页，改完点「从 NOS 重新获取」。"
                } else {
                    "密码保存在 Windows 凭据管理器，配置文件只记地址、账号与当前目录。只支持 https，本机回环可用 http。"
                },
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

    /// 岛仔音乐页点「播放」：从当前播放列表起播。返回是否成功起播。
    pub fn webdav_play_queue(&mut self, ctx: &egui::Context) -> bool {
        #[cfg(windows)]
        {
            // 已经在下载这一首：重复点播放只会从头再下一次，直接忽略。
            if self.webdav_buffering().is_some() {
                return true;
            }
            let playlist = self.webdav.playlist();
            if playlist.is_empty() {
                self.webdav.note = Some((false, "曲库还没读到歌，先到设置里「重新读取目录」".into()));
                self.say("曲库还没读到歌，先到设置里「重新读取目录」");
                return false;
            }
            let tracks: Vec<crate::webdav_player::Track> = playlist.iter()
                .map(|item| crate::webdav_player::Track { name: item.name.clone(), url: item.url.clone() })
                .collect();
            let cfg = self.webdav.config();
            if !cfg.ready() {
                self.webdav.note = Some((false, "WebDAV 未配置，请到设置里连接".into()));
                self.say("WebDAV 未配置，请到设置里连接");
                return false;
            }
            // 有用户名却没有本地密码：多半是没从 NOS 拉到，下载会 401，先说清楚。
            // 放在 engine 判断之外：引擎可能早就用空密码起过，重试也只会 401。
            let password = webdav::load_password(&cfg).unwrap_or_default();
            if !cfg.user.trim().is_empty() && password.trim().is_empty() {
                self.say("本地没有 WebDAV 密码，请到设置 → 音乐与歌词点「从 NOS 重新获取」");
                return false;
            }
            if self.webdav.engine.is_none() {
                match crate::webdav_player::Engine::start(ctx.clone(), cfg, password) {
                    Some(engine) => self.webdav.engine = Some(engine),
                    None => {
                        self.webdav.note = Some((false, "无法启动播放线程".into()));
                        self.say("无法启动播放线程，请稍后重试");
                        return false;
                    }
                }
            }
            if let Some(engine) = &self.webdav.engine {
                engine.play(tracks, 0);
                engine.set_order(self.webdav_order());
            }
            return true;
        }
        #[cfg(not(windows))]
        {
            let _ = ctx;
            false
        }
    }

    /// 岛仔音乐页点列表里的某一首：从该下标起播。
    pub fn webdav_play_index(&mut self, index: usize, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            let playlist = self.webdav.playlist();
            if index >= playlist.len() {
                return;
            }
            let tracks: Vec<crate::webdav_player::Track> = playlist.iter()
                .map(|item| crate::webdav_player::Track { name: item.name.clone(), url: item.url.clone() })
                .collect();
            let cfg = self.webdav.config();
            if !cfg.ready() {
                self.say("WebDAV 未配置，请到设置里连接");
                return;
            }
            let password = webdav::load_password(&cfg).unwrap_or_default();
            if !cfg.user.trim().is_empty() && password.trim().is_empty() {
                self.say("本地没有 WebDAV 密码，请到设置 → 音乐与歌词点「从 NOS 重新获取」");
                return;
            }
            if self.webdav.engine.is_none() {
                match crate::webdav_player::Engine::start(ctx.clone(), cfg, password) {
                    Some(engine) => self.webdav.engine = Some(engine),
                    None => {
                        self.say("无法启动播放线程，请稍后重试");
                        return;
                    }
                }
            }
            if let Some(engine) = &self.webdav.engine {
                engine.play(tracks, index);
                engine.set_order(self.webdav_order());
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (index, ctx);
        }
    }

    /// 岛上打开曲库面板时自动读一次目录（只试一次，失败靠设置页重试）。
    /// 密码只在此时从凭据管理器取，避免每帧读盘。
    pub fn webdav_ensure_listed(&mut self, ctx: &egui::Context) {
        if self.webdav.loaded || self.webdav.loading || self.webdav.auto_tried {
            return;
        }
        // 测试里不发起真实网络请求：单测只验界面与状态，联网由设置页手动触发。
        if cfg!(test) {
            return;
        }
        let cfg = self.webdav.config();
        if !cfg.ready() {
            // 配置还没到位（比如 NOS 下发的还没回来）：不算试过，等下一帧。
            return;
        }
        self.webdav.auto_tried = true;
        if self.webdav.password.is_empty() {
            self.webdav.password = webdav::load_password(&cfg).unwrap_or_default();
        }
        let target = if self.webdav.collection.trim().is_empty() {
            cfg.base().unwrap_or_default()
        } else {
            self.webdav.collection.clone()
        };
        if target.is_empty() {
            return;
        }
        let user = cfg.user.clone();
        let password = self.webdav.password.clone();
        self.webdav.loading = true;
        let paint = ctx.clone();
        self.webdav.spawn(move |tx| {
            let result = webdav::list(&target, &user, &password);
            let _ = tx.send(Msg::Listed(result));
            paint.request_repaint();
        });
    }

    /// 从另一台设备接手：曲目若在本地播放列表里就从原下标继续，否则单曲起播。
    pub fn webdav_takeover(&mut self, url: &str, track: &str, position: f64, ctx: &egui::Context) {
        let url = url.trim();
        if url.is_empty() {
            // 对方放的是系统播放器里的歌，拿不到可续播地址：只保留播放权，由用户自己开播。
            self.say("已接手播放；对方的曲目只能在它那台机器上续播");
            return;
        }
        #[cfg(windows)]
        {
            let playlist = self.webdav.playlist();
            let (tracks, index): (Vec<crate::webdav_player::Track>, usize) =
                match playlist.iter().position(|item| item.url == url) {
                    Some(index) => (
                        playlist.iter()
                            .map(|item| crate::webdav_player::Track { name: item.name.clone(), url: item.url.clone() })
                            .collect(),
                        index,
                    ),
                    None => (
                        vec![crate::webdav_player::Track { name: track.to_string(), url: url.to_string() }],
                        0,
                    ),
                };
            let cfg = self.webdav.config();
            if !cfg.ready() {
                self.say("WebDAV 未配置，请到设置里连接");
                return;
            }
            let password = webdav::load_password(&cfg).unwrap_or_default();
            if !cfg.user.trim().is_empty() && password.trim().is_empty() {
                self.say("本地没有 WebDAV 密码，请到设置 → 音乐与歌词点「从 NOS 重新获取」");
                return;
            }
            if self.webdav.engine.is_none() {
                match crate::webdav_player::Engine::start(ctx.clone(), cfg, password) {
                    Some(engine) => self.webdav.engine = Some(engine),
                    None => {
                        self.say("无法启动播放线程，请稍后重试");
                        return;
                    }
                }
            }
            if let Some(engine) = &self.webdav.engine {
                engine.play(tracks, index);
                engine.set_order(self.webdav_order());
                // 从对方停下的位置继续，而不是从头播。
                engine.seek(position.max(0.0));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (track, position, ctx);
        }
    }

    /// 当前播放顺序（顺序 / 乱序）。
    pub fn webdav_order(&self) -> crate::webdav_player::Order {
        if self.webdav.shuffle {
            crate::webdav_player::Order::Shuffle
        } else {
            crate::webdav_player::Order::Sequential
        }
    }

    /// 切换顺序 / 乱序，落盘并同步给运行中的播放线程。
    pub fn webdav_toggle_shuffle(&mut self) {
        self.webdav.shuffle = !self.webdav.shuffle;
        self.webdav.persist_queue();
        if let Some(engine) = &self.webdav.engine {
            engine.set_order(self.webdav_order());
        }
    }

    /// 后台自动拉取成功后的刷新：只读入新配置到界面，不打断正在进行的浏览。
    pub fn reload_webdav_config(&mut self) {
        let cfg = webdav::Config::load();
        self.webdav.url = cfg.url.clone();
        self.webdav.user = cfg.user.clone();
        self.webdav.from_nos = cfg.from_nos;
        self.webdav.from_nos_at = cfg.from_nos_at.clone();
        // 同源目录保留，换服务器则由 apply 清空：直接读回磁盘值。
        self.webdav.collection = cfg.dir.clone();
        self.webdav.entries.clear();
        self.webdav.loaded = false;
        // 换地址后要重新读一次目录，允许自动加载再来一次。
        self.webdav.auto_tried = false;
        self.webdav.password = webdav::load_password(&cfg).unwrap_or_default();
        self.webdav.password_loaded = true;
        self.webdav_saved = cfg;
        self.webdav.note = Some((true, "已从 NOS 更新 WebDAV 连接配置".into()));
    }

    /// 从 NOS 拉取 WebDAV 连接配置。地址与令牌取自课表共用的一份连接，
    /// 不另存配置；网络与凭据写入都在后台线程，界面只读结果。
    fn pull_webdav_from_nos(&mut self, ctx: &egui::Context) {
        let cfg = self.nos_config();
        if !cfg.ready() {
            self.webdav.note = Some((false, "请先在「课表与多端」里连接 NOS".into()));
            return;
        }
        self.webdav.busy = true;
        self.webdav.note = None;
        let paint = ctx.clone();
        self.webdav.spawn(move |tx| {
            let agent = crate::nos::http_agent();
            let result = crate::webdav_nos::pull(&agent, &cfg.base_url, &cfg.token);
            let _ = tx.send(Msg::Pulled(result));
            paint.request_repaint();
        });
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
            let result = webdav::list(&probe, &user, &password).map(|(_, entries)| entries.len());
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
        self.webdav.auto_tried = true;   // 设置页手动读过了，岛上不必再自动读一次
        self.webdav.error = None;
        let paint = ctx.clone();
        self.webdav.spawn(move |tx| {
            let result = webdav::list(&target, &user, &password);
            let _ = tx.send(Msg::Listed(result));
            paint.request_repaint();
        });
    }

    fn webdav_browser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // 还没取过目录时不占地方：点过「测试连接」或「重新读取目录」才展开。
        // 有缓存目录（上次启动留下的）也直接展开，避免看起来像没读到。
        if !self.webdav.loaded && self.webdav.error.is_none() && self.webdav.entries.is_empty() {
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
                    ui.horizontal(|ui| {
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
                        if entry.playable() && ui.small_button("＋序列").on_hover_text("加入播放序列").clicked() {
                            if !self.webdav.queue.iter().any(|item| item.url == entry.url) {
                                self.webdav.queue.push(entry.clone());
                                self.webdav.persist_queue();
                                self.webdav.note = Some((true, format!("已将「{}」加入播放序列", entry.name)));
                            }
                        }
                    });
                }
            });
        });
        if !self.webdav.queue.is_empty() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("播放序列").size(12.5).strong());
                ui.label(RichText::new(format!("{} 首", self.webdav.queue.len())).size(11.0).color(ui.visuals().weak_text_color()));
                if ui.small_button("播放序列").clicked() {
                    self.play_webdav_queue(ctx);
                }
                if ui.small_button("清空").clicked() {
                    self.webdav.queue.clear();
                    self.webdav.persist_queue();
                }
            });
            ui.label(RichText::new("有序列时岛仔只播序列；清空后退回按当前曲库目录播放。")
                .size(10.5).color(ui.visuals().weak_text_color()));
            for (index, item) in self.webdav.queue.clone().iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}. {}", index + 1, item.name)).size(11.5));
                    if ui.small_button("↑").clicked() && index > 0 { self.webdav.queue.swap(index, index - 1); self.webdav.persist_queue(); }
                    if ui.small_button("↓").clicked() && index + 1 < self.webdav.queue.len() { self.webdav.queue.swap(index, index + 1); self.webdav.persist_queue(); }
                    if ui.small_button("×").clicked() { self.webdav.queue.remove(index); self.webdav.persist_queue(); }
                });
            }
        }
    }

    #[cfg(windows)]
    fn play_webdav_queue(&mut self, ctx: &egui::Context) {
        let playlist: Vec<crate::webdav_player::Track> = self.webdav.queue.iter()
            .map(|item| crate::webdav_player::Track { name: item.name.clone(), url: item.url.clone() })
            .collect();
        if playlist.is_empty() { return; }
        if self.webdav.engine.is_none() {
            let cfg = self.webdav.config();
            let _ = cfg.save();
            match crate::webdav_player::Engine::start(ctx.clone(), cfg, self.webdav.password.clone()) {
                Some(engine) => self.webdav.engine = Some(engine),
                None => { self.webdav.note = Some((false, "无法启动播放线程".into())); return; }
            }
        }
        if let Some(engine) = &self.webdav.engine {
            engine.play(playlist, 0);
            engine.set_order(if self.webdav.shuffle {
                crate::webdav_player::Order::Shuffle
            } else {
                crate::webdav_player::Order::Sequential
            });
        }
    }

    #[cfg(not(windows))]
    fn play_webdav_queue(&mut self, _ctx: &egui::Context) {}

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
            engine.set_order(if self.webdav.shuffle {
                crate::webdav_player::Order::Shuffle
            } else {
                crate::webdav_player::Order::Sequential
            });
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
            use crate::webdav_player::Status;

            let Some(engine) = &self.webdav.engine else { return };
            let state = engine.snapshot();
            let Some(track) = state.current().cloned() else { return };
            let (artist, title) = crate::webdav::split_name(&track.name);
            let title = if title.is_empty() { track.name.clone() } else { title };
            let artist = if artist.is_empty() { "WebDAV 音乐".to_string() } else { artist };

            // 控制指令攒到卡片画完再发：闭包里同时读写 self.webdav 的其他字段。
            let mut toggle = false;
            let mut previous = false;
            let mut next = false;
            let mut seek = None;
            let mut volume = None;
            let mut draft_volume = self.webdav.volume_draft.unwrap_or(state.volume);

            settings_label(ui, "正在播放");
            settings_card(ui, |ui| {
                let ctx = ui.ctx().clone();
                let dark = ctx.style().visuals.dark_mode;
                let strong = settings_color(&ctx, Color32::from_gray(240), Color32::from_gray(28));
                let weak = settings_color(&ctx, Color32::from_gray(158), Color32::from_gray(112));

                ui.horizontal(|ui| {
                    let (cover, _) = ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
                    draw_cover(ui.painter(), cover, &title);
                    ui.add_space(11.0);
                    // 先量好右侧三个圆钮要占多宽，再让标题用剩下的宽度，
                    // 否则长文件名会把按钮挤出卡片。
                    let buttons = 30.0 + 36.0 + 30.0 + ui.spacing().item_spacing.x * 2.0;
                    let text_width = (ui.available_width() - buttons - 10.0).max(70.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(text_width, 44.0),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            ui.add(
                                egui::Label::new(RichText::new(&title).size(13.5).strong().color(strong)).truncate(),
                            );
                            let (line, color) = match &state.status {
                                Status::Downloading(progress) => {
                                    (format!("正在下载 {:.0}% · {artist}", progress * 100.0), weak)
                                }
                                Status::Error(error) => (error.clone(), Color32::from_rgb(214, 92, 72)),
                                _ => (artist.clone(), weak),
                            };
                            ui.add(egui::Label::new(RichText::new(line).size(11.0).color(color)).truncate());
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if transport_button(ui, 30.0, Glyph::Next, false, "下一首").clicked() {
                            next = true;
                        }
                        let tip = if state.playing() { "暂停" } else { "播放" };
                        let glyph = if state.playing() { Glyph::Pause } else { Glyph::Play };
                        if transport_button(ui, 36.0, glyph, true, tip).clicked() {
                            toggle = true;
                        }
                        if transport_button(ui, 30.0, Glyph::Prev, false, "上一首").clicked() {
                            previous = true;
                        }
                    });
                });

                ui.add_space(9.0);
                let duration = state.duration.max(0.1);
                let shown = self.webdav.draft.unwrap_or_else(|| state.live_position()).clamp(0.0, duration);
                let width = ui.available_width();
                let (bar, fraction) = slide_bar(ui, width, (shown / duration) as f32, 14.0, dark);
                if bar.is_pointer_button_down_on() || bar.dragged() {
                    self.webdav.draft = Some((fraction as f64 * duration).clamp(0.0, duration));
                }
                if bar.clicked() || bar.drag_stopped() {
                    seek = Some((fraction as f64 * duration).clamp(0.0, duration));
                    self.webdav.draft = None;
                }
                let shown = self.webdav.draft.unwrap_or(shown);
                ui.add_space(1.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(mmss(shown)).size(11.0).color(weak));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(mmss(duration)).size(11.0).color(weak));
                    });
                });

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("音量").size(11.0).color(weak));
                    let (bar, fraction) = slide_bar(ui, 96.0, draft_volume as f32 / 100.0, 12.0, dark);
                    if bar.is_pointer_button_down_on() || bar.dragged() {
                        draft_volume = (fraction * 100.0).round().clamp(0.0, 100.0) as u8;
                        self.webdav.volume_draft = Some(draft_volume);
                        volume = Some(draft_volume);
                    }
                    if bar.drag_stopped() {
                        self.webdav.volume_draft = None;
                    }
                    ui.label(RichText::new(format!("{draft_volume}%")).size(11.0).color(weak));
                });
            });

            if previous {
                engine.prev();
            }
            if toggle {
                engine.toggle();
            }
            if next {
                engine.next();
            }
            if let Some(position) = seek {
                engine.seek(position);
            }
            if let Some(level) = volume {
                engine.volume(level);
            }
            if seek.is_some() || volume.is_some() || toggle || previous || next {
                ui.ctx().request_repaint();
            }
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

/* ---------------- 播放条绘制 ---------------- */

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Prev,
    Play,
    Pause,
    Next,
}

/// 封面占位：用曲名生成稳定配色，和岛上没有封面时的色块是同一种颜色语言。
fn draw_cover(painter: &egui::Painter, rect: egui::Rect, title: &str) {
    let (top, _bottom) = crate::pages::cover_colors(title);
    painter.rect_filled(rect, 10.0, top);
    painter.rect_stroke(
        rect,
        10.0,
        egui::Stroke::new(1.0, Color32::from_white_alpha(30)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "♪",
        egui::FontId::proportional(20.0),
        Color32::from_white_alpha(225),
    );
}

/// 圆形按钮：播放键是实心主按钮，上一首/下一首悬停时才浮出圆底。
fn transport_button(ui: &mut egui::Ui, size: f32, glyph: Glyph, primary: bool, tip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let dark = ui.ctx().style().visuals.dark_mode;
    let hovered = response.hovered();
    let painter = ui.painter();
    if primary {
        let fill = if dark { Color32::from_gray(238) } else { Color32::from_gray(32) };
        let ink = if dark { Color32::from_gray(26) } else { Color32::WHITE };
        painter.circle_filled(rect.center(), size * 0.5, fill);
        // 三角形重心偏左，向右挪半像素看起来才居中。
        let offset = if glyph == Glyph::Play { egui::vec2(0.7, 0.0) } else { egui::Vec2::ZERO };
        draw_glyph(painter, rect.center() + offset, size * 0.42, glyph, ink);
    } else {
        if hovered {
            let background = if dark { Color32::from_white_alpha(22) } else { Color32::from_black_alpha(14) };
            painter.circle_filled(rect.center(), size * 0.5, background);
        }
        let ink = if dark { Color32::from_gray(228) } else { Color32::from_gray(48) };
        draw_glyph(painter, rect.center(), size * 0.5, glyph, ink);
    }
    response.on_hover_text(tip)
}

fn draw_glyph(painter: &egui::Painter, center: egui::Pos2, size: f32, glyph: Glyph, color: Color32) {
    let half = size * 0.5;
    match glyph {
        Glyph::Play => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + egui::vec2(-half * 0.52, -half),
                    center + egui::vec2(-half * 0.52, half),
                    center + egui::vec2(half * 0.74, 0.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        Glyph::Pause => {
            let bar = egui::vec2(half * 0.30, half * 1.05);
            painter.rect_filled(egui::Rect::from_center_size(center + egui::vec2(-half * 0.26, 0.0), bar), 1.4, color);
            painter.rect_filled(egui::Rect::from_center_size(center + egui::vec2(half * 0.26, 0.0), bar), 1.4, color);
        }
        Glyph::Prev | Glyph::Next => {
            let direction = if glyph == Glyph::Prev { -1.0 } else { 1.0 };
            let height = half * 0.78;
            let width = half * 0.72;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + egui::vec2(direction * width, 0.0),
                    center + egui::vec2(-direction * width * 0.12, -height),
                    center + egui::vec2(-direction * width * 0.12, height),
                ],
                color,
                egui::Stroke::NONE,
            ));
            painter.rect_filled(
                egui::Rect::from_center_size(
                    center + egui::vec2(direction * (width + 1.5), 0.0),
                    egui::vec2(2.2, height * 2.0),
                ),
                1.1,
                color,
            );
        }
    }
}

/// 细进度条：拖动时直接返回 0..1 的位置，松手由调用方决定是否跳转。
fn slide_bar(ui: &mut egui::Ui, width: f32, fraction: f32, height: f32, dark: bool) -> (egui::Response, f32) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
    let painter = ui.painter();
    let thickness = 4.0;
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), thickness));
    let base = if dark { Color32::from_gray(64) } else { Color32::from_gray(205) };
    let accent = if dark { Color32::from_gray(240) } else { Color32::from_gray(34) };
    painter.rect_filled(track, thickness * 0.5, base);
    let mut fraction = fraction.clamp(0.0, 1.0);
    let active = response.is_pointer_button_down_on() || response.dragged();
    if active {
        if let Some(pointer) = response.interact_pointer_pos() {
            fraction = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
        }
    }
    if fraction > 0.0 {
        let filled_width = (track.width() * fraction).max(thickness);
        let filled = egui::Rect::from_min_size(track.left_top(), egui::vec2(filled_width, thickness));
        painter.rect_filled(filled, thickness * 0.5, accent);
    }
    if active || response.hovered() {
        let head = egui::pos2(track.left() + track.width() * fraction, track.center().y);
        painter.circle_filled(head, 5.5, accent);
    }
    (response, fraction)
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

#[cfg(test)]
mod playlist_tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            url: format!("https://nas.local/music/{name}"),
            is_dir,
            bytes: 1,
        }
    }

    #[test]
    fn playlist_prefers_the_saved_queue() {
        let mut ui = Ui::default();
        ui.queue = vec![entry("queued.mp3", false)];
        ui.entries = vec![entry("dir.mp3", false)];
        assert_eq!(ui.playlist().len(), 1);
        assert_eq!(ui.playlist()[0].name, "queued.mp3");
    }

    #[test]
    fn empty_queue_falls_back_to_the_current_directory() {
        // 这就是「曲库里有歌、岛上却显示 0 首」的那个 bug：没加序列时要回落到目录。
        let mut ui = Ui::default();
        ui.queue.clear();
        ui.entries = vec![entry("Music", true), entry("a.mp3", false), entry("b.flac", false), entry("c.jpg", false)];
        let playlist = ui.playlist();
        assert_eq!(playlist.len(), 2, "只保留音频，目录与图片不算");
        assert_eq!(playlist[0].name, "a.mp3");
    }

    #[test]
    fn both_empty_gives_an_empty_playlist() {
        let mut ui = Ui::default();
        ui.queue.clear();
        ui.entries.clear();
        assert!(ui.playlist().is_empty());
    }
}
