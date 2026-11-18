//! NVIDIA 显卡读数（NVML）。
//!
//! `nvml.dll` 随 NVIDIA 驱动装进 `System32`，普通用户进程即可调用，
//! 不需要管理员、不需要 WDDM 性能计数器 —— 任务管理器里那格「GPU 温度」
//! 走的是驱动上报的 WDDM 数据，NVML 是同一份数据的直连版本，读到的值一致。
//!
//! 全部按 C ABI 动态加载：没有 NVML 的机器（A 卡/核显）只是读不到，不会报错。
//! 读写都在采样线程内完成，`Drop` 时 `nvmlShutdown`。

use std::ffi::c_void;

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::Foundation::HMODULE;

use crate::monitor::GpuInfo;

const NVML_SUCCESS: i32 = 0;
/// `nvmlTemperatureSensors_t::NVML_TEMPERATURE_GPU`
const NVML_TEMPERATURE_GPU: u32 = 0;
/// `nvmlClockType_t::NVML_CLOCK_GRAPHICS`
const NVML_CLOCK_GRAPHICS: u32 = 0;

#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

type FnInit = unsafe extern "C" fn() -> i32;
type FnShutdown = unsafe extern "C" fn() -> i32;
type FnCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type FnTemp = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
type FnU32 = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
type FnClock = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
type FnMem = unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> i32;

/// 一次初始化得到的函数表 + 选中的显卡句柄。
pub struct GpuReader {
    shutdown: FnShutdown,
    temp: FnTemp,
    power: FnU32,
    enforced_limit: Option<FnU32>,
    mangaged_limit: Option<FnU32>,
    fan: Option<FnU32>,
    clock: Option<FnClock>,
    memory: Option<FnMem>,
    device: *mut c_void,
    /// 显卡名（只在诊断日志里用）。
    pub name: String,
}

// 句柄与函数表都只在采样线程里使用；显式声明以便放进与 PDH 相同的持有方式。
unsafe impl Send for GpuReader {}

unsafe fn symbol<T: Copy>(module: HMODULE, name: windows::core::PCSTR) -> Option<T> {
    let proc = GetProcAddress(module, name)?;
    Some(std::mem::transmute_copy(&proc))
}

impl GpuReader {
    /// 打开 NVML 并选中第一块能报温度的显卡。任一步失败都返回 None（界面显示 `--`）。
    pub fn open() -> Option<Self> {
        unsafe {
            let module = LoadLibraryW(w!("nvml.dll")).ok()?;
            let init: FnInit = symbol(module, s!("nvmlInit_v2"))?;
            if init() != NVML_SUCCESS {
                return None;
            }
            let count: FnCount = symbol(module, s!("nvmlDeviceGetCount_v2"))?;
            let get_handle: FnHandle = symbol(module, s!("nvmlDeviceGetHandleByIndex_v2"))?;
            let get_name: unsafe extern "C" fn(*mut c_void, *mut i8, u32) -> i32 =
                symbol(module, s!("nvmlDeviceGetName"))?;
            let temp: FnTemp = symbol(module, s!("nvmlDeviceGetTemperature"))?;
            let power: FnU32 = symbol(module, s!("nvmlDeviceGetPowerUsage"))?;
            let shutdown: FnShutdown = symbol(module, s!("nvmlShutdown"))?;

            let mut total = 0u32;
            if count(&mut total) != NVML_SUCCESS || total == 0 {
                let _ = shutdown();
                return None;
            }
            for index in 0..total.min(8) {
                let mut device: *mut c_void = std::ptr::null_mut();
                if get_handle(index, &mut device) != NVML_SUCCESS || device.is_null() {
                    continue;
                }
                let mut celsius = 0u32;
                if temp(device, NVML_TEMPERATURE_GPU, &mut celsius) != NVML_SUCCESS {
                    continue;
                }
                let mut raw = [0i8; 96];
                let name = if get_name(device, raw.as_mut_ptr(), raw.len() as u32) == NVML_SUCCESS {
                    let bytes: Vec<u8> = raw.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
                    String::from_utf8_lossy(&bytes).trim().to_string()
                } else {
                    String::new()
                };
                return Some(Self {
                    shutdown,
                    temp,
                    power,
                    enforced_limit: symbol(module, s!("nvmlDeviceGetEnforcedPowerLimit")),
                    mangaged_limit: symbol(module, s!("nvmlDeviceGetPowerManagementLimit")),
                    fan: symbol(module, s!("nvmlDeviceGetFanSpeed")),
                    clock: symbol(module, s!("nvmlDeviceGetClockInfo")),
                    memory: symbol(module, s!("nvmlDeviceGetMemoryInfo")),
                    device,
                    name,
                });
            }
            let _ = shutdown();
            None
        }
    }

    pub fn sample(&self) -> GpuInfo {
        let mut out = GpuInfo::default();
        unsafe {
            let mut celsius = 0u32;
            if (self.temp)(self.device, NVML_TEMPERATURE_GPU, &mut celsius) == NVML_SUCCESS {
                out.temp_c = Some(celsius as f32);
            }
            let mut mw = 0u32;
            if (self.power)(self.device, &mut mw) == NVML_SUCCESS && mw > 0 {
                out.power_w = Some(mw as f32 / 1000.0);
            }
            for getter in [self.enforced_limit, self.mangaged_limit].into_iter().flatten() {
                let mut limit = 0u32;
                if getter(self.device, &mut limit) == NVML_SUCCESS && limit > 0 {
                    out.limit_w = Some(limit as f32 / 1000.0);
                    break;
                }
            }
            if let Some(fan) = self.fan {
                let mut percent = 0u32;
                if fan(self.device, &mut percent) == NVML_SUCCESS {
                    out.fan_pct = Some(percent);
                }
            }
            if let Some(clock) = self.clock {
                let mut mhz = 0u32;
                if clock(self.device, NVML_CLOCK_GRAPHICS, &mut mhz) == NVML_SUCCESS && mhz > 0 {
                    out.clock_mhz = Some(mhz);
                }
            }
            if let Some(memory) = self.memory {
                let mut info = NvmlMemory::default();
                if memory(self.device, &mut info) == NVML_SUCCESS && info.total > 0 {
                    out.vram_used_bytes = Some(info.used);
                    out.vram_total_bytes = Some(info.total);
                }
            }
        }
        out
    }
}

impl Drop for GpuReader {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.shutdown)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只在有 NVIDIA 卡的机器上有意义；没有卡时静默跳过，避免 CI 依赖硬件。
    #[test]
    fn nvml_reads_temperature_when_available() {
        let Some(reader) = GpuReader::open() else {
            return;
        };
        let info = reader.sample();
        if let Some(temp) = info.temp_c {
            assert!((0.0..=120.0).contains(&temp), "显卡温度异常：{temp}");
        }
        if let Some(w) = info.power_w {
            assert!((0.0..=1200.0).contains(&w), "显卡功耗异常：{w}");
        }
    }
}
