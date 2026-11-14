//! Windows 系统指标采集层（P2）。
//!
//! 只做「读系统 → 喂给 `monitor` 纯逻辑 → 把 `MonitorSnapshot` 送回界面」这三件事，
//! 不绘制任何 UI。与 `system::observe` 同样的后台模型：命名线程 + `sync_channel(1)`
//! + `PollClock` 节流 + 变化时 `ctx.request_repaint()`。模块关闭时 `Service` 被丢弃，
//! `stop` 置位，线程在 50ms 内退出，不残留采样线程。
//!
//! 采集来源：
//! - 网速/流量：`GetIfTable2` + `MIB_IF_ROW2`（64 位计数器，修掉参考实现的 32 位回绕）
//! - CPU：`GetSystemTimes`（kernel 含 idle，busy = kernel + user - idle）
//! - 内存：`GlobalMemoryStatusEx`
//! - 本地日期：`chrono::Local`
//!
//! P2 的可观测性走数据目录 `diagnostics/monitor.log`（每 30 秒覆盖写一次），
//! 阈值提醒本期只写日志、不弹气泡（气泡属于 P4）。

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
    Arc,
};
use std::time::{Duration, Instant};

use eframe::egui::Context;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{FILETIME, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_FMT_COUNTERVALUE,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
};
use windows::Win32::System::Power::{
    CallNtPowerInformation, ProcessorInformation, PROCESSOR_POWER_INFORMATION,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use crate::monitor::{
    format_size, format_speed, Alert, Counters, HistoryStore, MonitorConfig, MonitorSnapshot,
    RateCalculator, SavePolicy, ThresholdWatcher,
};
use crate::poll_clock::PollClock;

fn filetime(t: &FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

/// 活动网卡（供设置页下拉）。`index` 为 InterfaceIndex，0 保留给「全部网卡」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub index: u32,
    pub name: String,
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 枚举活动、非回环网卡；调用方（Shell）在启动时取一次即可，网卡热插拔后重启生效。
pub fn list_interfaces() -> Vec<Interface> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).0 != NO_ERROR.0 || table.is_null() {
            return Vec::new();
        }
        let count = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        let mut out = Vec::new();
        for row in rows {
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK
                || row.OperStatus != IfOperStatusUp
                || row.InterfaceIndex == 0
            {
                continue;
            }
            let name = wide_to_string(&row.Alias);
            out.push(Interface {
                index: row.InterfaceIndex,
                name: if name.is_empty() { format!("网卡 {}", row.InterfaceIndex) } else { name },
            });
        }
        FreeMibTable(table as *const _);
        out
    }
}

/// 所有活动、非回环网卡的累计字节求和（等价于 `read_counters_for(0)`）。
pub fn read_counters() -> Option<Counters> {
    read_counters_for(0)
}

/// `selection = 0` 统计全部活动网卡，否则只统计该 InterfaceIndex。
/// 断开的接口计数不变，计入基准不会造成速率尖峰；计数器倒退由 `RateCalculator` 兜底。
/// 选中的网卡被拔出时返回 None，调用方按 0 速率处理，恢复后基准会自动重建。
pub fn read_counters_for(selection: u32) -> Option<Counters> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).0 != NO_ERROR.0 || table.is_null() {
            return None;
        }
        let count = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        let mut out = Counters::default();
        let mut found = false;
        for row in rows {
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK || row.OperStatus != IfOperStatusUp {
                continue;
            }
            if selection != 0 && row.InterfaceIndex != selection {
                continue;
            }
            found = true;
            out.down_bytes = out.down_bytes.saturating_add(row.InOctets);
            out.up_bytes = out.up_bytes.saturating_add(row.OutOctets);
        }
        FreeMibTable(table as *const _);
        if selection != 0 && !found {
            return None;
        }
        Some(out)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub kernel: u64,
    pub user: u64,
}

impl CpuTimes {
    /// `kernel` 已包含 `idle`，因此 busy = (kernel + user) - idle。
    /// 计数器倒退或间隔内总时间为 0 时返回 None，避免除零与负数。
    pub fn percent_since(prev: &Self, cur: &Self) -> Option<f32> {
        let total_prev = prev.kernel.saturating_add(prev.user);
        let total_cur = cur.kernel.saturating_add(cur.user);
        let delta_total = total_cur.checked_sub(total_prev)?;
        let delta_busy = total_cur
            .saturating_sub(cur.idle)
            .checked_sub(total_prev.saturating_sub(prev.idle))?;
        if delta_total == 0 {
            return None;
        }
        Some((delta_busy as f32 * 100.0 / delta_total as f32).clamp(0.0, 100.0))
    }
}

pub fn read_cpu_times() -> Option<CpuTimes> {
    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        GetSystemTimes(
            Some(&mut idle as *mut FILETIME),
            Some(&mut kernel as *mut FILETIME),
            Some(&mut user as *mut FILETIME),
        )
        .ok()?;
        Some(CpuTimes {
            idle: filetime(&idle),
            kernel: filetime(&kernel),
            user: filetime(&user),
        })
    }
}

/// (已用物理内存, 物理内存总量)，单位字节。
pub fn read_memory() -> Option<(u64, u64)> {
    unsafe {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut status).ok()?;
        Some((
            status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
            status.ullTotalPhys,
        ))
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

// ---------------------------------------------------------------- P5：扩展硬件指标

/// PDH / 电源 API 读取的扩展指标。虚拟机、无独显或计数器被禁用时对应项为 None，
/// 界面显示 `--` 而不是 0，避免把「读不到」误报成「没占用」。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Hardware {
    pub gpu_percent: Option<f32>,
    pub disk_percent: Option<f32>,
    pub cpu_mhz: Option<u32>,
    /// 全盘读写吞吐，MB/s（PDH 计数器给的是字节/秒）。
    pub disk_read_mbps: Option<f32>,
    pub disk_write_mbps: Option<f32>,
}

/// 注册表里的 CPU 型号，例如 `13th Gen Intel(R) Core(TM) i7-13700H`。
/// 只读一次即可，取不到就返回 None，界面退回只显示计算机名。
pub fn cpu_name() -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 256];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"),
            w!("ProcessorNameString"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status.0 != 0 {
        return None;
    }
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    let name = String::from_utf16_lossy(&buf[..len]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// 当前 CPU 频率（取所有逻辑处理器 CurrentMhz 的最大值）。
pub fn read_cpu_mhz() -> Option<u32> {
    let mut info = [PROCESSOR_POWER_INFORMATION::default(); 64];
    let len = (info.len() * std::mem::size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
    let status = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(info.as_mut_ptr() as *mut core::ffi::c_void),
            len,
        )
    };
    if !status.is_ok() {
        return None;
    }
    info.iter().filter(|p| p.CurrentMhz > 0).map(|p| p.CurrentMhz).max()
}

fn pdh_single(counter: isize) -> Option<f64> {
    unsafe {
        let mut value = PDH_FMT_COUNTERVALUE::default();
        if PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut value) != 0 {
            return None;
        }
        if value.CStatus != 0 {
            return None;
        }
        let v = value.Anonymous.doubleValue;
        v.is_finite().then_some(v)
    }
}

/// GPU Engine 是多实例计数器：取所有实例中的最大值，而不是求和
/// （同一块显卡的多个引擎会同时计数，相加会虚高到几百 %）。
fn pdh_array_max(counter: isize) -> Option<f32> {
    unsafe {
        let mut size: u32 = 0;
        let mut count: u32 = 0;
        if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None)
            != PDH_MORE_DATA
            || size == 0
        {
            return None;
        }
        let unit = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // 用同类型 Vec 而非 Vec<u8>，保证 PDH 写入时结构体对齐正确。
        let mut buffer: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = vec![
            PDH_FMT_COUNTERVALUE_ITEM_W::default();
            (size as usize + unit - 1) / unit + 1
        ];
        if PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &mut size,
            &mut count,
            Some(buffer.as_mut_ptr()),
        ) != 0
        {
            return None;
        }
        let max = buffer[..(count as usize).min(buffer.len())]
            .iter()
            .filter(|item| item.FmtValue.CStatus == 0)
            .map(|item| item.FmtValue.Anonymous.doubleValue)
            .filter(|v| v.is_finite())
            .fold(0.0f64, f64::max);
        Some(max.clamp(0.0, 100.0) as f32)
    }
}

/// 英文计数器名（`PdhAddEnglishCounterW`）避开非英文 Windows 的本地化路径。
pub struct PdhCounters {
    query: isize,
    gpu: Option<isize>,
    disk: Option<isize>,
    read: Option<isize>,
    write: Option<isize>,
}

impl PdhCounters {
    pub fn new() -> Self {
        unsafe {
            let mut query: isize = 0;
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return Self { query: 0, gpu: None, disk: None, read: None, write: None };
            }
            let add = |path: PCWSTR| -> Option<isize> {
                let mut handle: isize = 0;
                if PdhAddEnglishCounterW(query, path, 0, &mut handle) == 0 {
                    Some(handle)
                } else {
                    None
                }
            };
            let gpu = add(w!("\\GPU Engine(*)\\Utilization Percentage"));
            let disk = add(w!("\\PhysicalDisk(_Total)\\% Idle Time"));
            let read = add(w!("\\PhysicalDisk(_Total)\\Disk Read Bytes/sec"));
            let write = add(w!("\\PhysicalDisk(_Total)\\Disk Write Bytes/sec"));
            // 比率型计数器至少需要一次 collect 才能产出有效值。
            let _ = PdhCollectQueryData(query);
            Self { query, gpu, disk, read, write }
        }
    }

    pub fn sample(&self) -> Hardware {
        let mut out = Hardware { cpu_mhz: read_cpu_mhz(), ..Default::default() };
        if self.query == 0 {
            return out;
        }
        unsafe {
            if PdhCollectQueryData(self.query) == 0 {
                out.disk_percent = self
                    .disk
                    .and_then(pdh_single)
                    .map(|idle| (100.0 - idle as f32).clamp(0.0, 100.0));
                out.gpu_percent = self.gpu.and_then(pdh_array_max);
                out.disk_read_mbps = self.read.and_then(pdh_single).map(to_mbps);
                out.disk_write_mbps = self.write.and_then(pdh_single).map(to_mbps);
            }
        }
        out
    }
}

/// 字节/秒 → MB/s。计数器刚打开时可能给出负值或极大值，统一收敛到合理区间。
fn to_mbps(bytes_per_sec: f64) -> f32 {
    (bytes_per_sec / 1_048_576.0).clamp(0.0, 1_000_000.0) as f32
}

impl Drop for PdhCounters {
    fn drop(&mut self) {
        if self.query != 0 {
            unsafe {
                let _ = PdhCloseQuery(self.query);
            }
        }
    }
}

/// 人类可读的诊断文本，P2 阶段用它代替界面展示。
pub fn probe_text(
    snap: &MonitorSnapshot,
    cfg: &MonitorConfig,
    alerts: &[Alert],
    days: usize,
) -> String {
    let d = &cfg.display;
    let mut s = format!("岛仔 {} · 系统监控诊断\n", env!("CARGO_PKG_VERSION"));
    s.push_str(&format!(
        "进程：{}   采样间隔：{} ms   采集时间：{}\n",
        std::process::id(),
        cfg.interval_ms,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    s.push_str(&format!(
        "下行：{}   上行：{}\n",
        format_speed(snap.down_bps, d),
        format_speed(snap.up_bps, d)
    ));
    s.push_str(&format!(
        "CPU：{}   内存：{} / {} ({})\n",
        snap.cpu_percent
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "--".into()),
        format_size(snap.mem_used_bytes, d),
        format_size(snap.mem_total_bytes, d),
        snap.mem_percent()
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "--".into())
    ));
    s.push_str(&format!(
        "GPU：{}   磁盘：{}   CPU 频率：{}   磁盘读写：{}/s · {}/s\n",
        snap.gpu_percent.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "--".into()),
        snap.disk_percent.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "--".into()),
        snap.cpu_mhz.map(|v| format!("{v} MHz")).unwrap_or_else(|| "--".into()),
        snap.disk_read_mbps.map(|v| format!("{v:.1} MB")).unwrap_or_else(|| "--".into()),
        snap.disk_write_mbps.map(|v| format!("{v:.1} MB")).unwrap_or_else(|| "--".into())
    ));
    s.push_str(&format!(
        "今日：下行 {} / 上行 {}（合计 {}）   历史天数：{}\n",
        format_size(snap.today_down_bytes, d),
        format_size(snap.today_up_bytes, d),
        format_size(snap.today_total(), d),
        days
    ));
    s.push_str(&format!(
        "告警：{}\n",
        if alerts.is_empty() {
            "无".to_string()
        } else {
            alerts
                .iter()
                .map(|a| a.message(d))
                .collect::<Vec<_>>()
                .join("；")
        }
    ));
    s
}

fn write_probe(text: &str) {
    let dir = crate::model::config_dir().join("diagnostics");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("monitor.log"), text);
    }
}

/// 一次性读取真实机器数据，供人工核对任务管理器/资源监视器数值。
pub fn probe() -> String {
    let cfg = MonitorConfig::load();
    let counters = read_counters().unwrap_or_default();
    let cpu = read_cpu_times();
    let (used, total) = read_memory().unwrap_or((0, 0));
    let snap = MonitorSnapshot {
        mem_used_bytes: used,
        mem_total_bytes: total,
        ..Default::default()
    };
    let mut text = probe_text(&snap, &cfg, &[], 0);
    text.push_str(&format!(
        "原始网卡计数：down={} up={}\nCPU 原始时间：{cpu:?}\n",
        counters.down_bytes, counters.up_bytes
    ));
    text
}

/// 后台采样服务。丢弃即停线程（模块开关关闭时 `reconcile_modules` 会把它置 None）。
/// 告警走独立通道，不塞进 `MonitorSnapshot`（后者要保持 Copy）。
pub struct Service {
    rx: Receiver<MonitorSnapshot>,
    alerts: Receiver<String>,
    stop: Arc<AtomicBool>,
}

impl Service {
    pub fn start(ctx: Context, cfg: MonitorConfig) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let (alert_tx, alerts) = mpsc::sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        // Lane 上限 2 允许旧线程退出与新实例交接。
        let _ = crate::resources::SYSTEM_MONITOR
            .spawn(move || run(ctx, cfg, tx, alert_tx, flag));
        Self { rx, alerts, stop }
    }

    pub fn try_recv(&self) -> Option<MonitorSnapshot> {
        self.rx.try_recv().ok()
    }

    /// 阈值告警文案；冷却与「持续越限只报一次」由 `ThresholdWatcher` 保证。
    pub fn try_alert(&self) -> Option<String> {
        self.alerts.try_recv().ok()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(ctx: Context, cfg: MonitorConfig, tx: SyncSender<MonitorSnapshot>, alert_tx: SyncSender<String>, stop: Arc<AtomicBool>) {
    let interval = cfg.interval();
    let mut rate = RateCalculator::new();
    let mut history = HistoryStore::load();
    let mut save = SavePolicy::new();
    let mut watcher = ThresholdWatcher::new(Duration::from_secs(cfg.thresholds.cooldown_secs));
    let mut cpu_last: Option<CpuTimes> = None;
    let mut clock = PollClock::default();
    let mut prev_at = Instant::now();
    let mut probe_at = prev_at;
    let mut alerts: Vec<Alert> = Vec::new();
    let mut last = MonitorSnapshot::default();
    // P5：PDH 查询句柄绑在采样线程内，随线程一起创建/释放。
    let pdh = PdhCounters::new();

    while !stop.load(Ordering::Relaxed) {
        if !clock.ready(interval) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        clock = PollClock::now();
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(prev_at);
        prev_at = now;

        let counters = read_counters_for(cfg.interface_index).unwrap_or_default();
        let sample = rate.step(counters, elapsed);

        let cpu = read_cpu_times();
        let cpu_percent = cpu.and_then(|cur| {
            cpu_last
                .map(|prev| CpuTimes::percent_since(&prev, &cur))
                .flatten()
        });
        cpu_last = cpu;
        let (used, total) = read_memory().unwrap_or((0, 0));
        let hardware = pdh.sample();

        let date = today();
        history.add_delta(&date, sample.delta);
        let day = history.today(&date);
        let snap = MonitorSnapshot {
            down_bps: sample.speeds.down_bps,
            up_bps: sample.speeds.up_bps,
            cpu_percent,
            mem_used_bytes: used,
            mem_total_bytes: total,
            today_down_bytes: day.map(|d| d.down_bytes).unwrap_or(0),
            today_up_bytes: day.map(|d| d.up_bytes).unwrap_or(0),
            gpu_percent: hardware.gpu_percent,
            disk_percent: hardware.disk_percent,
            cpu_mhz: hardware.cpu_mhz,
            disk_read_mbps: hardware.disk_read_mbps,
            disk_write_mbps: hardware.disk_write_mbps,
        };

        if let Some(alert) = watcher.check(
            now,
            snap.today_total(),
            cpu_percent,
            snap.mem_percent(),
            &cfg.thresholds,
        ) {
            let text = alert.message(&cfg.display);
            alerts.clear();
            alerts.push(alert);
            // P4：告警交给外壳弹托盘气泡；冷却与「持续越限只报一次」已在 watcher 里处理。
            let _ = alert_tx.try_send(text);
            ctx.request_repaint();
        }
        // ExitProcess 不会走 Drop，历史流量必须靠策略定期落盘。
        if save.should_save(now, snap.today_total()) {
            let _ = history.save();
        }
        if now.saturating_duration_since(probe_at) >= Duration::from_secs(30) {
            write_probe(&probe_text(&snap, &cfg, &alerts, history.days().len()));
            probe_at = now;
            alerts.clear();
        }
        if tx.try_send(snap).is_ok() {
            ctx.request_repaint();
        }
        last = snap;
    }
    let _ = history.save();
    write_probe(&probe_text(&last, &cfg, &alerts, history.days().len()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_combines_high_and_low_dwords() {
        let t = FILETIME {
            dwLowDateTime: 0x1234_5678,
            dwHighDateTime: 0x0000_00AB,
        };
        assert_eq!(filetime(&t), 0xAB_1234_5678);
    }

    #[test]
    fn cpu_percent_is_busy_over_total() {
        let prev = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 50,
        };
        let cur = CpuTimes {
            idle: 110,
            kernel: 210,
            user: 80,
        };
        assert_eq!(CpuTimes::percent_since(&prev, &cur), Some(75.0));
    }

    #[test]
    fn cpu_percent_rejects_rollback_and_zero_interval() {
        let prev = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 50,
        };
        let rollback = CpuTimes {
            idle: 100,
            kernel: 190,
            user: 50,
        };
        assert_eq!(CpuTimes::percent_since(&prev, &rollback), None);
        assert_eq!(CpuTimes::percent_since(&prev, &prev), None);
    }

    #[test]
    fn interface_list_and_selection_are_consistent() {
        let list = list_interfaces();
        let mut seen = std::collections::HashSet::new();
        for i in &list {
            assert!(i.index != 0, "0 保留给「全部网卡」");
            assert!(!i.name.is_empty());
            assert!(seen.insert(i.index), "InterfaceIndex 不应重复");
        }
        let all = read_counters_for(0);
        assert!(all.is_some() || list.is_empty());
        if let Some(first) = list.first() {
            let _ = read_counters_for(first.index);
        }
        // 选中的网卡不存在时返回 None，调用方按 0 速率处理，不会误把全部流量算进去。
        assert!(read_counters_for(u32::MAX).is_none());
    }

    #[test]
    fn hardware_counters_degrade_gracefully() {
        let h = PdhCounters::new().sample();
        // 虚拟机 / 无独显 / 计数器被禁用时允许 None，只验证范围与「不 panic」。
        for v in [h.gpu_percent, h.disk_percent].into_iter().flatten() {
            assert!((0.0..=100.0).contains(&v));
        }
        if let Some(mhz) = h.cpu_mhz {
            assert!(mhz > 0);
        }
        let _ = read_cpu_mhz();
    }

    #[test]
    fn probe_reads_real_machine_without_panicking() {
        let text = probe();
        assert!(text.contains("系统监控诊断"));
        assert!(text.contains("原始网卡计数"));
        assert!(text.contains("CPU 原始时间"));
        println!("{text}");
    }
}
