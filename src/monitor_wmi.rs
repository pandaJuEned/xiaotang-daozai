//! WMI（COM）读数：ACPI 热区。
//!
//! 走 `root\WMI` 的 `MSAcpi_ThermalZoneTemperature`。桌面主板通常一个热区都没有，
//! 笔记本才有；部分机型要求管理员，所以提权助手会再试一次。
//!
//! 盘温本想走 `root\Microsoft\Windows\Storage` 的
//! `MSFT_PhysicalDiskToStorageReliabilityCounter`，但实测该提供程序只实现了 MI 接口：
//! 关联类不支持 WQL 枚举、实例的 `__PATH` 也是空的（`Get-PhysicalDisk` 能读，
//! 是因为它走 MI API），所以 WMI COM 这条路拿不到盘温，不接入。
//!
//! 只能在采样线程里创建与使用：`IWbemServices` 不是 `Send`/`Sync`。

use windows::core::{BSTR, GUID, PCWSTR, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY,
};

use crate::monitor::{temperature_from_kelvin_tenths, MAX_TEMPERATURE_SENSORS, TemperatureSensor};

/// `CLSID_WbemLocator`
const CLSID_WBEM_LOCATOR: GUID = GUID::from_u128(0x4590_f811_1d3a_11d0_891f_00aa_004b_2e24);
/// `RPC_C_AUTHN_WINNT`（Win32::System::Rpc 里的常量，这里只需要数值）。
const AUTHN_WINNT: u32 = 10;
/// `RPC_C_AUTHZ_NONE`
const AUTHZ_NONE: u32 = 0;
const WMI_NAMESPACE: &str = "ROOT\\WMI";
/// `IEnumWbemClassObject::Next` 的无限等待。
const INFINITE_TIMEOUT: i32 = -1;

pub struct Wmi {
    root: Option<IWbemServices>,
    /// 本线程是否由我们完成了 `CoInitializeEx`，决定要不要配对 `CoUninitialize`。
    com: bool,
}

impl Wmi {
    pub fn new() -> Self {
        let mut com = false;
        unsafe {
            // MTA：与采样线程上的 WinRT 套间一致。已经是 MTA 时返回 S_FALSE，同样算成功。
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() {
                com = true;
            }
            let root = connect(WMI_NAMESPACE);
            Self { root, com }
        }
    }

    /// ACPI 热区读数；系统不提供该类时返回空。
    pub fn acpi_zones(&self) -> [Option<TemperatureSensor>; MAX_TEMPERATURE_SENSORS] {
        let mut out = [None; MAX_TEMPERATURE_SENSORS];
        let Some(services) = &self.root else {
            return out;
        };
        unsafe {
            let mut found: Vec<TemperatureSensor> = Vec::new();
            for zone in query(
                services,
                "SELECT InstanceName, CurrentTemperature FROM MSAcpi_ThermalZoneTemperature",
            ) {
                let Some(raw) = prop_i32(&zone, "CurrentTemperature") else {
                    continue;
                };
                let Some(celsius) = temperature_from_kelvin_tenths(raw as f64) else {
                    continue;
                };
                let name = prop_str(&zone, "InstanceName").unwrap_or_default();
                found.push(sensor_from(&name, celsius));
            }
            found.sort_by(|a, b| b.celsius.unwrap_or(0.0).total_cmp(&a.celsius.unwrap_or(0.0)));
            for (slot, sensor) in out.iter_mut().zip(found.into_iter().take(MAX_TEMPERATURE_SENSORS)) {
                *slot = Some(sensor);
            }
        }
        out
    }
}

impl Drop for Wmi {
    fn drop(&mut self) {
        self.root = None;
        if self.com {
            unsafe { CoUninitialize() };
        }
    }
}

fn sensor_from(name: &str, celsius: f32) -> TemperatureSensor {
    let mut buf = [0u16; 24];
    for (slot, ch) in buf.iter_mut().zip(name.encode_utf16()) {
        *slot = ch;
    }
    TemperatureSensor { name: buf, celsius: Some(celsius) }
}

unsafe fn connect(namespace: &str) -> Option<IWbemServices> {
    let locator: IWbemLocator =
        CoCreateInstance(&CLSID_WBEM_LOCATOR, None, CLSCTX_INPROC_SERVER).ok()?;
    let empty = BSTR::new();
    let services: IWbemServices = locator
        .ConnectServer(&BSTR::from(namespace), &empty, &empty, &empty, 0, &empty, None)
        .ok()?;
    // 存储提供程序要代表调用方做事，不设代理空白会以 WBEM_E_ACCESS_DENIED（0x80041003）拒绝。
    let _ = CoSetProxyBlanket(
        &services,
        AUTHN_WINNT,
        AUTHZ_NONE,
        None,
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    );
    Some(services)
}

unsafe fn query(services: &IWbemServices, wql: &str) -> Vec<IWbemClassObject> {
    let mut out = Vec::new();
    let Ok(enumerator) = services.ExecQuery(
        &BSTR::from("WQL"),
        &BSTR::from(wql),
        WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
        None,
    ) else {
        return out;
    };
    next_objects(&enumerator, &mut out);
    out
}

unsafe fn next_objects(enumerator: &IEnumWbemClassObject, out: &mut Vec<IWbemClassObject>) {
    loop {
        let mut batch: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0u32;
        if enumerator.Next(INFINITE_TIMEOUT, &mut batch, &mut returned).is_err() || returned == 0 {
            return;
        }
        if let Some(object) = batch[0].take() {
            out.push(object);
        }
    }
}

unsafe fn name_ptr(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn prop_str(object: &IWbemClassObject, name: &str) -> Option<String> {
    let mut value = VARIANT::default();
    let wide = name_ptr(name);
    object.Get(PCWSTR(wide.as_ptr()), 0, &mut value, None, None).ok()?;
    BSTR::try_from(&value).ok().map(|v| v.to_string())
}

unsafe fn prop_i32(object: &IWbemClassObject, name: &str) -> Option<i32> {
    let mut value = VARIANT::default();
    let wide = name_ptr(name);
    object.Get(PCWSTR(wide.as_ptr()), 0, &mut value, None, None).ok()?;
    i32::try_from(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 桌面主板通常一个热区都没有，读到就要落在合理区间。
    #[test]
    fn acpi_zones_are_plausible_when_present() {
        let wmi = Wmi::new();
        for zone in wmi.acpi_zones().iter().flatten() {
            let celsius = zone.celsius.expect("热区读数不应是空值");
            assert!(
                (-40.0..=125.0).contains(&celsius),
                "热区温度异常：{} {celsius}",
                zone.name()
            );
        }
    }
}
