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
use windows::Devices::Power::Battery;
use windows::System::Power::BatteryStatus as WinRtBatteryStatus;
use windows::Win32::System::Power::{
    CallNtPowerInformation, GetSystemPowerStatus, ProcessorInformation, SYSTEM_POWER_STATUS,
    PROCESSOR_POWER_INFORMATION,
};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use crate::monitor::{
    format_size, format_speed, Alert, Counters, GpuInfo, HistoryStore, MonitorConfig,
    MonitorSnapshot, PowerSnapshot, RateCalculator, SavePolicy, TemperatureSensor, ThresholdWatcher,
    MAX_TEMPERATURE_SENSORS,
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
    /// ACPI 热区温度（摄氏度）；常见平台无需额外驱动或管理员权限。
    pub temperature_c: Option<f32>,
    /// 全部热区读数，按温度从高到低排列。
    pub temperature_sensors: [Option<TemperatureSensor>; MAX_TEMPERATURE_SENSORS],
    /// Power Meter 计数器给出的交流/整机功率；不支持时为 None。
    pub ac_power_w: Option<f32>,
    /// 全盘读写吞吐，MB/s（PDH 计数器给的是字节/秒）。
    pub disk_read_mbps: Option<f32>,
    pub disk_write_mbps: Option<f32>,
    /// 显卡温度/功耗/显存（NVML，零提权）。
    pub gpu: GpuInfo,
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

/// 采集线程独占一个 WinRT MTA 套间；未初始化成功时不配对调用 RoUninitialize。
struct WinRtApartment(bool);
impl WinRtApartment {
    fn enter() -> Self {
        Self(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}
impl Drop for WinRtApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

/// 电池使用与充放电功率。WinRT 的 AggregateBattery 不需要管理员权限。
pub fn read_power() -> PowerSnapshot {
    let mut out = PowerSnapshot::default();
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut status).is_ok() {
            out.ac_online = Some(status.ACLineStatus != 0);
            if status.BatteryLifePercent != 255 {
                out.battery_percent = Some(status.BatteryLifePercent as f32);
            }
            if status.BatteryLifeTime != u32::MAX {
                out.runtime_secs = Some(status.BatteryLifeTime);
            }
        }
    }
    if let Ok(battery) = Battery::AggregateBattery() {
        if let Ok(report) = battery.GetReport() {
            let mw = |value: windows::Foundation::IReference<i32>| value.Value().ok().map(|v| v as f32 / 1000.0);
            out.charge_w = report.ChargeRateInMilliwatts().ok().and_then(mw).filter(|v| *v > 0.0);
            out.remaining_wh = report.RemainingCapacityInMilliwattHours().ok().and_then(mw);
            let status = report.Status().ok();
            out.charging = status.map(|s| s == WinRtBatteryStatus::Charging);
            out.discharging = status.map(|s| s == WinRtBatteryStatus::Discharging);
        }
    }
    out
}

/// WinRT 不直接给放电功率；用剩余容量的时间差换算，0 间隔或充电时不出数。
#[derive(Default)]
struct PowerTracker {
    last: Option<(Instant, f32)>,
}
impl PowerTracker {
    fn step(&mut self, now: Instant, mut power: PowerSnapshot) -> PowerSnapshot {
        if let (Some(remaining), Some((prev_at, prev))) = (power.remaining_wh, self.last) {
            let elapsed = now.saturating_duration_since(prev_at).as_secs_f32();
            if elapsed > 0.25 && power.discharging == Some(true) && remaining <= prev {
                power.discharge_w = Some((prev - remaining) * 3600.0 / elapsed);
            }
        }
        self.last = power.remaining_wh.map(|remaining| (now, remaining));
        power
    }
}

/// 读取 wildcard 计数器的有效实例并取最大值。没有有效实例时返回 None。
/// GPU 引擎与热区都适用：前者要避免多引擎求和虚高，后者要取最热的热区。
fn pdh_array_peak(counter: isize) -> Option<f64> {
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
        buffer[..(count as usize).min(buffer.len())]
            .iter()
            .filter(|item| item.FmtValue.CStatus == 0)
            .map(|item| item.FmtValue.Anonymous.doubleValue)
            .filter(|v| v.is_finite())
            .fold(None, |max, v| Some(max.map_or(v, |m| m.max(v))))
    }
}

/// GPU Engine 占用率：百分比范围固定，异常值收敛到 0..100。
fn pdh_array_max(counter: isize) -> Option<f32> {
    pdh_array_peak(counter).map(|v| v.clamp(0.0, 100.0) as f32)
}

unsafe fn copy_pdh_name(name: windows::core::PWSTR) -> [u16; 24] {
    let mut out = [0u16; 24];
    if name.0.is_null() {
        return out;
    }
    for (i, slot) in out[..23].iter_mut().enumerate() {
        let c = *name.0.add(i);
        if c == 0 {
            break;
        }
        *slot = c;
    }
    out
}

/// 读取所有热区实例，保留名称与读数并按温度降序排列；系统只暴露一路时其余为空。
fn pdh_temperatures(counter: isize) -> [Option<TemperatureSensor>; MAX_TEMPERATURE_SENSORS] {
    let mut out = [None; MAX_TEMPERATURE_SENSORS];
    unsafe {
        let mut size: u32 = 0;
        let mut count: u32 = 0;
        if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None)
            != PDH_MORE_DATA
            || size == 0
        {
            return out;
        }
        let unit = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
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
            return out;
        }
        let mut found: Vec<TemperatureSensor> = buffer[..(count as usize).min(buffer.len())]
            .iter()
            .filter(|item| item.FmtValue.CStatus == 0)
            .filter_map(|item| {
                let value = item.FmtValue.Anonymous.doubleValue;
                crate::monitor::temperature_from_kelvin_tenths(value).map(|celsius| {
                    TemperatureSensor { name: copy_pdh_name(item.szName), celsius: Some(celsius) }
                })
            })
            .collect();
        found.sort_by(|a, b| {
            b.celsius.unwrap_or(f32::NEG_INFINITY).total_cmp(&a.celsius.unwrap_or(f32::NEG_INFINITY))
        });
        for (slot, sensor) in out.iter_mut().zip(found.into_iter().take(MAX_TEMPERATURE_SENSORS)) {
            *slot = Some(sensor);
        }
    }
    out
}

/// 英文计数器名（`PdhAddEnglishCounterW`）避开非英文 Windows 的本地化路径。
pub struct PdhCounters {
    query: isize,
    gpu: Option<isize>,
    disk: Option<isize>,
    read: Option<isize>,
    write: Option<isize>,
    temperature: Option<isize>,
    ac_power: Option<isize>,
}

impl PdhCounters {
    pub fn new() -> Self {
        unsafe {
            let mut query: isize = 0;
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return Self { query: 0, gpu: None, disk: None, read: None, write: None, temperature: None, ac_power: None };
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
            let temperature = add(w!("\\Thermal Zone Information(*)\\High Precision Temperature"));
            let ac_power = add(w!("\\Power Meter(_Total)\\Power"));
            // 比率型计数器至少需要一次 collect 才能产出有效值。
            let _ = PdhCollectQueryData(query);
            Self { query, gpu, disk, read, write, temperature, ac_power }
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
                if let Some(counter) = self.temperature {
                    out.temperature_sensors = pdh_temperatures(counter);
                    out.temperature_c = out.temperature_sensors.iter().flatten().next().and_then(|s| s.celsius);
                }
                out.ac_power_w = self.ac_power.and_then(pdh_single).map(|v| v as f32).filter(|v| *v > 0.0);
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
    let temps = snap
        .temperature_sensors
        .iter()
        .flatten()
        .map(|s| {
            let label = crate::monitor::thermal_zone_label(&s.name());
            format!("{label} {:.1}°C", s.celsius.unwrap_or(0.0))
        })
        .collect::<Vec<_>>();
    let temp_text = if temps.is_empty() { "--".to_string() } else { temps.join(" · ") };
    s.push_str(&format!(
        "GPU：{}   磁盘：{}   CPU 频率：{}   温度：{}   磁盘读写：{}/s · {}/s\n",
        snap.gpu_percent.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "--".into()),
        snap.disk_percent.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "--".into()),
        snap.cpu_mhz.map(|v| format!("{v} MHz")).unwrap_or_else(|| "--".into()),
        temp_text,
        snap.disk_read_mbps.map(|v| format!("{v:.1} MB")).unwrap_or_else(|| "--".into()),
        snap.disk_write_mbps.map(|v| format!("{v:.1} MB")).unwrap_or_else(|| "--".into())
    ));
    let vram = match (snap.gpu.vram_used_bytes, snap.gpu.vram_total_bytes) {
        (Some(used), Some(total)) => format!(
            "{:.1}/{:.1} GB",
            used as f64 / 1_073_741_824.0,
            total as f64 / 1_073_741_824.0
        ),
        _ => "--".to_string(),
    };
    s.push_str(&format!(
        "显卡：温度 {}   功耗 {}   上限 {}   风扇 {}   核心 {}   显存 {}\n",
        snap.gpu.temp_c.map(|v| format!("{v:.1}°C")).unwrap_or_else(|| "--".into()),
        snap.gpu.power_w.map(|v| format!("{v:.1}W")).unwrap_or_else(|| "--".into()),
        snap.gpu.limit_w.map(|v| format!("{v:.0}W")).unwrap_or_else(|| "--".into()),
        snap.gpu.fan_pct.map(|v| format!("{v}%")).unwrap_or_else(|| "--".into()),
        snap.gpu.clock_mhz.map(|v| format!("{v} MHz")).unwrap_or_else(|| "--".into()),
        vram
    ));
    s.push_str(&format!(
        "提权助手：{}\n",
        if snap.elevated_live { "运行中" } else { "未运行" }
    ));
    let p = &snap.power;
    let watts = |v: Option<f32>| v.map(|v| format!("{v:.1}W")).unwrap_or_else(|| "--".into());
    s.push_str(&format!(
        "电源：交流 {}   电池 {}   充电 {}   放电 {}   交流功率 {}   剩余 {}\n",
        p.ac_online.map(|v| if v { "已接入" } else { "未接入" }).unwrap_or("--"),
        p.battery_percent.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "--".into()),
        watts(p.charge_w),
        watts(p.discharge_w),
        watts(p.ac_power_w),
        p.remaining_wh.map(|v| format!("{v:.1}Wh")).unwrap_or_else(|| "--".into()),
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
    let _winrt = WinRtApartment::enter();
    let hardware = PdhCounters::new().sample();
    let gpu = crate::monitor_gpu::GpuReader::open().map(|r| r.sample()).unwrap_or_default();
    let mut sensors = hardware.temperature_sensors;
    if sensors.iter().all(|s| s.is_none()) {
        sensors = crate::monitor_wmi::Wmi::new().acpi_zones();
    }
    let mut power = read_power();
    power.ac_power_w = hardware.ac_power_w;
    let snap = MonitorSnapshot {
        mem_used_bytes: used,
        mem_total_bytes: total,
        gpu_percent: hardware.gpu_percent,
        disk_percent: hardware.disk_percent,
        cpu_mhz: hardware.cpu_mhz,
        temperature_c: sensors.iter().flatten().next().and_then(|s| s.celsius),
        temperature_sensors: sensors,
        power,
        disk_read_mbps: hardware.disk_read_mbps,
        disk_write_mbps: hardware.disk_write_mbps,
        gpu,
        elevated_live: crate::monitor_elevated::read_zones().is_some(),
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
    pub fn start(ctx: Context, cfg: MonitorConfig, elevated_sensors: bool) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let (alert_tx, alerts) = mpsc::sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        // Lane 上限 2 允许旧线程退出与新实例交接。
        let _ = crate::resources::SYSTEM_MONITOR
            .spawn(move || run(ctx, cfg, elevated_sensors, tx, alert_tx, flag));
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

fn run(
    ctx: Context,
    cfg: MonitorConfig,
    elevated_sensors: bool,
    tx: SyncSender<MonitorSnapshot>,
    alert_tx: SyncSender<String>,
    stop: Arc<AtomicBool>,
) {
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
    // P5：PDH 查询句柄与 WinRT 套间绑在采样线程内，随线程一起创建/释放。
    let pdh = PdhCounters::new();
    let _winrt = WinRtApartment::enter();
    let mut power_tracker = PowerTracker::default();
    // 显卡（NVML）是零提权通道，打不开时整段退化为 None。
    let gpu = crate::monitor_gpu::GpuReader::open();
    let mut elevated: [Option<TemperatureSensor>; MAX_TEMPERATURE_SENSORS] =
        [None; MAX_TEMPERATURE_SENSORS];
    let mut elevated_live = false;
    let mut elevated_at: Option<Instant> = None;

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
        let mut power = power_tracker.step(now, read_power());
        power.ac_power_w = hardware.ac_power_w;

        // 显卡每秒都读：NVML 调用是内存级的，比一次 PDH 采集还便宜。
        let gpu_info = gpu.as_ref().map(|reader| reader.sample()).unwrap_or_default();
        // 提权助手的读数落在一个 JSON 文件里，5 秒读一次即可。
        if elevated_sensors
            && elevated_at.map_or(true, |at| now.duration_since(at) >= Duration::from_secs(5))
        {
            elevated = [None; MAX_TEMPERATURE_SENSORS];
            elevated_live = false;
            if let Some(zones) = crate::monitor_elevated::read_zones() {
                elevated_live = true;
                for (slot, (name, celsius)) in elevated.iter_mut().zip(zones.into_iter()) {
                    let mut buf = [0u16; 24];
                    for (cell, ch) in buf.iter_mut().zip(name.encode_utf16()) {
                        *cell = ch;
                    }
                    *slot = Some(TemperatureSensor { name: buf, celsius: Some(celsius) });
                }
            }
            elevated_at = Some(now);
        } else if !elevated_sensors {
            elevated = [None; MAX_TEMPERATURE_SENSORS];
            elevated_live = false;
        }
        // ACPI 热区优先用 PDH（不需要提权），读不到时才用助手的结果。
        let mut sensors = hardware.temperature_sensors;
        if sensors.iter().all(|s| s.is_none()) {
            sensors = elevated;
        }
        let temperature_c = sensors
            .iter()
            .flatten()
            .next()
            .and_then(|s| s.celsius)
            .or(hardware.temperature_c);

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
            temperature_c,
            temperature_sensors: sensors,
            power,
            disk_read_mbps: hardware.disk_read_mbps,
            disk_write_mbps: hardware.disk_write_mbps,
            gpu: gpu_info,
            elevated_live,
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
        if let Some(celsius) = h.temperature_c {
            assert!((-40.0..=125.0).contains(&celsius), "热区温度异常：{celsius}");
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
