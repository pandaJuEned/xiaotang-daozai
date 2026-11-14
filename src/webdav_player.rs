//! WebDAV 曲库的播放引擎：下载到本地缓存 → rodio 解码 → 系统默认输出设备。
//!
//! 只做「一次一首」：整首下载到缓存再解码，换来可靠的跳转与不中断的播放；
//! 进度、切歌、音量都在这个线程里完成，界面只读 `State` 快照。
//!
//! 岛仔原本只是播放器的遥控器（SMTC），这条路是唯一真正在进程内出声的地方，
//! 所以它完全独立于 `smtc`：不写 `NowPlaying`，也不影响岛上的歌词与封面。

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::webdav;

/// 一首待播的曲目。
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    /// 正在下载，0..1。
    Downloading(f32),
    Playing,
    Paused,
    Error(String),
}

impl Default for Status {
    fn default() -> Self {
        Status::Idle
    }
}

/// 给界面看的快照。这里**不含整份播放列表**：列表可能有几百首，
/// 每帧克隆一次字符串会拖慢设置页。
#[derive(Clone, Debug, Default)]
pub struct State {
    pub current: Option<Track>,
    pub index: usize,
    pub total: usize,
    pub status: Status,
    pub position: f64,
    pub duration: f64,
    pub volume: u8,
}

impl State {
    pub fn current(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    pub fn playing(&self) -> bool {
        matches!(self.status, Status::Playing)
    }
}

enum Cmd {
    Play(Vec<Track>, usize),
    Toggle,
    Next,
    Prev,
    Seek(f64),
    Volume(u8),
    Stop,
}

pub struct Engine {
    tx: mpsc::Sender<Cmd>,
    state: Arc<Mutex<State>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // 只发停止信号，不 join：worker 可能正卡在下载里，join 会冻住界面线程。
        // 线程会在 200ms 内看到 Stop 自行退出。
        let _ = self.tx.send(Cmd::Stop);
        let _ = self.worker.take();
    }
}

impl Engine {
    pub fn start(ctx: eframe::egui::Context, cfg: webdav::Config, password: String) -> Option<Self> {
        let state = Arc::new(Mutex::new(State { volume: 80, ..Default::default() }));
        let (tx, rx) = mpsc::channel();
        let shared = state.clone();
        let worker = crate::resources::WEBDAV.spawn(move || {
            run(rx, shared, ctx, cfg, password);
        })?;
        Some(Self { tx, state, worker: Some(worker) })
    }

    /// 不阻塞界面：锁被播放线程短暂占住时返回默认快照。
    pub fn snapshot(&self) -> State {
        self.state.lock().map(|state| state.clone()).unwrap_or_default()
    }

    pub fn play(&self, playlist: Vec<Track>, index: usize) {
        // 先把「要播哪首」写进状态，再交给线程：设备打不开或下载失败时，
        // 播放条仍然能显示这一首和错误信息。
        if let Ok(mut current) = self.state.lock() {
            current.total = playlist.len();
            current.index = index;
            current.current = playlist.get(index).cloned();
            current.position = 0.0;
            current.duration = 0.0;
        }
        let _ = self.tx.send(Cmd::Play(playlist, index));
    }

    pub fn toggle(&self) {
        let _ = self.tx.send(Cmd::Toggle);
    }

    pub fn next(&self) {
        let _ = self.tx.send(Cmd::Next);
    }

    pub fn prev(&self) {
        let _ = self.tx.send(Cmd::Prev);
    }

    pub fn seek(&self, position: f64) {
        let _ = self.tx.send(Cmd::Seek(position));
    }

    pub fn volume(&self, volume: u8) {
        let _ = self.tx.send(Cmd::Volume(volume));
    }
}

fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("daozai-webdav")
}

/// 文件名用 URL 的哈希，避免中文与特殊字符带来的路径问题；保留扩展名给解码器参考。
fn cache_path(track: &Track) -> PathBuf {
    let mut hash: u64 = 1469598103934665603;
    for byte in track.url.bytes() {
        hash = (hash ^ byte as u64).wrapping_mul(1099511628211);
    }
    let ext = Path::new(&track.name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase();
    cache_dir().join(format!("{hash:016x}.{ext}"))
}

/// 只留当前这一首，其余旧缓存删掉。
fn clean_cache(keep: &Path) {
    let Some(dir) = keep.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != keep {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn set_status(state: &Mutex<State>, status: Status, ctx: &eframe::egui::Context) {
    if let Ok(mut current) = state.lock() {
        current.status = status;
    }
    ctx.request_repaint();
}

/// 播放线程持有的播放列表：界面看不到整份列表，切歌只改下标。
struct Session {
    playlist: Vec<Track>,
    /// 当前曲目的装载时刻，用来区分「还没出声」和「已经播完」。
    loaded: Option<Instant>,
}

fn run(
    rx: mpsc::Receiver<Cmd>,
    state: Arc<Mutex<State>>,
    ctx: eframe::egui::Context,
    cfg: webdav::Config,
    password: String,
) {
    let sink = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(sink) => sink,
        Err(error) => {
            // 设备打不开也要留在循环里收命令，否则 play() 会写进没人读的通道，
            // 界面上连错误都看不到。
            set_status(&state, Status::Error(format!("无法打开音频设备：{error}")), &ctx);
            idle_until_stop(rx);
            return;
        }
    };
    let player = rodio::Player::connect_new(&sink.mixer());
    player.set_volume(0.8);
    let mut session = Session { playlist: Vec::new(), loaded: None };

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(cmd) => {
                if !handle(cmd, &player, &state, &ctx, &cfg, &password, &mut session) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // 一首播完自动接下一首；只有一首时停在 Idle。
        if let Some(started) = session.loaded {
            if started.elapsed() > Duration::from_secs(1) && player.empty() {
                if session.playlist.len() > 1 {
                    let _ = handle(Cmd::Next, &player, &state, &ctx, &cfg, &password, &mut session);
                } else {
                    session.loaded = None;
                    set_status(&state, Status::Idle, &ctx);
                }
            }
        }

        // 回写播放进度与暂停状态。
        if let Ok(mut current) = state.lock() {
            if matches!(current.status, Status::Playing | Status::Paused) {
                current.position = player.get_pos().as_secs_f64();
                let playing = !player.is_paused();
                current.status = if playing { Status::Playing } else { Status::Paused };
            }
        }
    }
    player.stop();
}

fn idle_until_stop(rx: mpsc::Receiver<Cmd>) {
    while let Ok(cmd) = rx.recv() {
        if matches!(cmd, Cmd::Stop) {
            break;
        }
    }
}

/// 返回 false 表示线程该退出了。
fn handle(
    cmd: Cmd,
    player: &rodio::Player,
    state: &Arc<Mutex<State>>,
    ctx: &eframe::egui::Context,
    cfg: &webdav::Config,
    password: &str,
    session: &mut Session,
) -> bool {
    match cmd {
        Cmd::Stop => return false,
        Cmd::Play(playlist, index) => {
            session.playlist = playlist;
            move_to(player, state, ctx, cfg, password, session, index);
        }
        Cmd::Next => {
            if !session.playlist.is_empty() {
                let current = state.lock().map(|state| state.index).unwrap_or(0);
                let next = (current + 1) % session.playlist.len();
                move_to(player, state, ctx, cfg, password, session, next);
            }
        }
        Cmd::Prev => {
            if !session.playlist.is_empty() {
                let current = state.lock().map(|state| state.index).unwrap_or(0);
                let prev = (current + session.playlist.len() - 1) % session.playlist.len();
                move_to(player, state, ctx, cfg, password, session, prev);
            }
        }
        Cmd::Toggle => {
            if player.is_paused() {
                player.play();
            } else {
                player.pause();
            }
        }
        Cmd::Seek(position) => {
            let position = position.max(0.0);
            if player.try_seek(Duration::from_secs_f64(position)).is_ok() {
                if let Ok(mut current) = state.lock() {
                    current.position = position;
                }
            }
        }
        Cmd::Volume(volume) => {
            let volume = volume.min(100);
            player.set_volume(volume as f32 / 100.0);
            if let Ok(mut current) = state.lock() {
                current.volume = volume;
            }
        }
    }
    true
}

/// 切到指定下标：下载 → 解码 → 播放。
fn move_to(
    player: &rodio::Player,
    state: &Arc<Mutex<State>>,
    ctx: &eframe::egui::Context,
    cfg: &webdav::Config,
    password: &str,
    session: &mut Session,
    index: usize,
) {
    let Some(track) = session.playlist.get(index).cloned() else { return };
    if let Ok(mut current) = state.lock() {
        current.index = index;
        current.total = session.playlist.len();
        current.current = Some(track.clone());
        current.position = 0.0;
        current.duration = 0.0;
    }
    session.loaded = None;
    player.stop();
    set_status(state, Status::Downloading(0.0), ctx);

    let path = cache_path(&track);
    let base = cfg.base().unwrap_or_default();
    let mut last = 0.0f32;
    let result = webdav::download(&base, &track.url, &cfg.user, password, &path, &mut |progress| {
        // 每 2% 刷新一次，避免每 64KB 都抢锁。
        if progress - last >= 0.02 {
            last = progress;
            set_status(state, Status::Downloading(progress), ctx);
        }
    });
    if let Err(error) = result {
        set_status(state, Status::Error(error), ctx);
        return;
    }
    clean_cache(&path);

    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            set_status(state, Status::Error(format!("缓存文件读取失败：{error}")), ctx);
            return;
        }
    };
    let source = match rodio::Decoder::try_from(file) {
        Ok(source) => source,
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            set_status(state, Status::Error(format!("这个格式解不了：{error}")), ctx);
            return;
        }
    };
    use rodio::Source as _;
    let duration = source.total_duration().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    if let Ok(mut current) = state.lock() {
        current.duration = duration;
        current.position = 0.0;
    }
    player.append(source);
    player.play();
    session.loaded = Some(Instant::now());
    set_status(state, Status::Playing, ctx);
}
