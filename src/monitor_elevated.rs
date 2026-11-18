//! 提权传感器助手（可选的「深度传感器」开关）。
//!
//! 为什么单独开一个进程：岛仔主体刻意不申请管理员（不为读数引入 UAC），
//! 而 ACPI 热区在部分机型上只有提权后才返回数据。所以开关打开时用
//! `ShellExecuteW(runas)` 再起一个**自己的**进程，由它把读数写进
//! `%APPDATA%\Daozai\elevated-sensors.json`，主体只读文件，权限边界留在进程外。
//!
//! 助手每 5 秒覆盖写一次；主体只在文件 20 秒内更新过时才采信，
//! 助手被关掉（或 UAC 被拒）后读数会自动失效，不会显示陈旧温度。

use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 助手进程的命令行标记；`main` 在最开头识别它，不进 UI。
pub const FLAG: &str = "--elevated-sensors";
const WRITE_INTERVAL: Duration = Duration::from_secs(5);
/// 读数有效期：助手被结束后最多再采信这么久。
const STALE_AFTER_MS: u128 = 20_000;

fn snapshot_path() -> std::path::PathBuf {
    crate::model::config_dir().join("elevated-sensors.json")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 当前进程是否由「提权助手」启动。
pub fn is_helper_invocation() -> bool {
    std::env::args().any(|arg| arg == FLAG)
}

/// 助手主循环：每 5 秒读一次 ACPI 热区并落盘；父进程消失即退出。
pub fn run_helper() -> ! {
    let parent = parent_pid();
    let wmi = crate::monitor_wmi::Wmi::new();
    loop {
        if parent_gone(parent) {
            let _ = std::fs::remove_file(snapshot_path());
            std::process::exit(0);
        }
        let zones: Vec<serde_json::Value> = wmi
            .acpi_zones()
            .iter()
            .flatten()
            .map(|sensor| {
                serde_json::json!({
                    "name": sensor.name(),
                    "celsius": sensor.celsius,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "pid": std::process::id(),
            "parent": parent,
            "at": now_ms() as u64,
            "zones": zones,
        });
        if let Some(dir) = snapshot_path().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(snapshot_path(), payload.to_string());
        std::thread::sleep(WRITE_INTERVAL);
    }
}

/// 父进程 PID：`--elevated-sensors 1234`。
fn parent_pid() -> u32 {
    std::env::args()
        .skip_while(|arg| arg != FLAG)
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn parent_gone(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0,
        };
        use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(handle) => {
                let exited = WaitForSingleObject(handle, 0) == WAIT_OBJECT_0;
                let _ = CloseHandle(handle);
                exited
            }
            // 提权助手与被提权的岛仔可能不在同一权限级别，OpenProcess 会返回「拒绝访问」。
            // 这种情况说明进程还在，只有「参数无效」才是真的没有这个进程。
            Err(_) => GetLastError() == ERROR_INVALID_PARAMETER,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// 用 `runas` 拉起助手。用户点了「否」时返回 Err（调用方把开关拨回去）。
pub fn launch() -> Result<(), String> {
    #[cfg(windows)]
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let exe = std::env::current_exe().map_err(|_| "找不到岛仔自身路径".to_string())?;
        let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let params_w: Vec<u16> = format!("{FLAG} {}", std::process::id())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let operation: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
        let result = ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(params_w.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        );
        // ShellExecuteW 约定：返回值 > 32 表示成功。
        if result.0 as usize > 32 {
            Ok(())
        } else {
            Err("没有获得管理员授权".to_string())
        }
    }
    #[cfg(not(windows))]
    {
        Err("仅 Windows 支持".to_string())
    }
}

/// 读助手写下的热区；文件过期或不存在时返回 None。
pub fn read_zones() -> Option<Vec<(String, f32)>> {
    let raw = std::fs::read(snapshot_path()).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let at = value.get("at")?.as_u64()? as u128;
    if now_ms().saturating_sub(at) > STALE_AFTER_MS {
        return None;
    }
    let zones = value.get("zones")?.as_array()?;
    let mut out = Vec::new();
    for zone in zones {
        let name = zone.get("name").and_then(|v| v.as_str()).unwrap_or("热区");
        let celsius = zone.get("celsius").and_then(|v| v.as_f64())? as f32;
        if celsius.is_finite() && (-40.0..=125.0).contains(&celsius) {
            out.push((name.to_string(), celsius));
        }
    }
    Some(out)
}

/// 助手是否在跑（文件新鲜且 PID 还活着）。设置页用它显示状态。
pub fn helper_alive() -> bool {
    read_zones().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_snapshot_is_ignored() {
        let path = snapshot_path();
        let backup = std::fs::read(&path).ok();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let old = serde_json::json!({"at": 1u64, "zones": [{"name": "热区 1", "celsius": 50.0}]});
        std::fs::write(&path, old.to_string()).unwrap();
        assert!(read_zones().is_none(), "过期的助手读数不应被采信");
        let fresh = serde_json::json!({
            "at": now_ms() as u64,
            "zones": [{"name": "热区 1", "celsius": 50.0}],
        });
        std::fs::write(&path, fresh.to_string()).unwrap();
        let zones = read_zones().expect("新鲜读数应可读");
        assert_eq!(zones.len(), 1);
        assert!((zones[0].1 - 50.0).abs() < 0.01);
        match backup {
            Some(raw) => {
                let _ = std::fs::write(&path, raw);
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}
