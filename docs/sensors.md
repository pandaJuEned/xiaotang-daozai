# 传感器能力清单与未接入项

记录岛仔（Windows）系统监控里**已经接入**与**评估后暂不接入**的传感器，以及每条的实测依据。
新增传感器前先看这里，避免重复蹚已经走不通的路。

所有数字来自 2026-11-18 在开发机（ASUS 桌面主板 / Ryzen 5 3600X / RTX 5060 Ti / Windows 11 25H2 26200）上的实测。

## 已接入（全部零提权）

| 指标 | 数据源 | 代码位置 | 备注 |
|---|---|---|---|
| CPU 占用 / 内存 | `GetSystemTimes`、`GlobalMemoryStatusEx` | `monitor_win.rs` | 无外部依赖 |
| 网速 | `GetIfTable2`（64 位计数器） | `monitor_win.rs` | 修掉了 32 位回绕 |
| GPU 占用 / 磁盘占用 / CPU 频率 | PDH 计数器 | `monitor_win.rs` | `\GPU Engine`、`\PhysicalDisk`、`CallNtPowerInformation` |
| 显卡温度 / 功耗 / 上限 / 风扇 / 核心频率 / 显存 | NVML（`System32\nvml.dll`，随 NVIDIA 驱动安装） | `monitor_gpu.rs` | 与任务管理器 GPU 页同源；非 N 卡静默退化为 `--` |
| ACPI 热区温度 | PDH `\Thermal Zone Information(*)\High Precision Temperature` | `monitor_win.rs` | 桌面主板通常是 0 个热区 |
| ACPI 热区（提权助手补充） | WMI `root\WMI:MSAcpi_ThermalZoneTemperature` | `monitor_wmi.rs` + `monitor_elevated.rs` | 设置里「深度传感器」开关，默认关 |
| 电池 / 交流状态 / 充放电功率 | WinRT 电池报告 + `GetSystemPowerStatus` | `monitor_win.rs` | 桌面无电池时全为 `--` |

补充两点实现细节：

- **WMI COM 必须设代理空白**：不调 `CoSetProxyBlanket`（`AUTHN_WINNT=10`、`RPC_C_IMP_LEVEL_IMPERSONATE`）时，
  `IEnumWbemClassObject::Next` 会返回 `0x80041003`（访问被拒），而 `ConnectServer` 本身是成功的，很容易误判成"没有数据"。
- **提权助手不是必需的**：主体刻意不申请管理员，需要提权的读数由 `--elevated-sensors <父进程PID>` 助手进程写 JSON 文件，
  主体只读文件（20 秒过期）。父进程存活判断里，`OpenProcess` 失败只有 `ERROR_INVALID_PARAMETER(87)` 才算父进程没了，
  `ACCESS_DENIED(5)` 说明进程还在（提权子系统看非提权父进程就是这种情况）。

## 未接入：磁盘温度

三条路都试过，结论是**当前不接入**。

### 1. `IOCTL_STORAGE_QUERY_PROPERTY` + `StorageDeviceTemperatureProperty(22)`

对 `\\.\PhysicalDrive0..2` 直接查询，三块盘全部返回 `ERROR_INVALID_FUNCTION(1)`；
同一路径下 `StorageDeviceProperty(0)` 查询正常（返回 396 字节），说明 IOCTL 管道本身没问题，
是这台机器的 stornvme/ata 驱动没有实现温度属性。换机器可能可用，但不可依赖。

### 2. WMI COM：`MSFT_PhysicalDiskToStorageReliabilityCounter`

`root\Microsoft\Windows\Storage` 命名空间的可靠性计数器（温度、通电小时、磨损）**只实现了 MI 接口**，WMI COM 侧拿不到：

- `SELECT * FROM MSFT_StorageReliabilityCounter` → 0 条（不支持枚举）
- 关联类 `MSFT_PhysicalDiskToStorageReliabilityCounter` 枚举 → 0 条
- 磁盘实例的 `__PATH` → 空字符串（只有自定义 `ObjectId`）
- `ASSOCIATORS OF {...}` 各种路径写法 → `0x8004103A`（对象路径无效）/ `0x80041017`（查询无效）

`Get-PhysicalDisk | Get-StorageReliabilityCounter` 能读到（CN700 44°C、WD40EZRZ 32°C、WD3200AAJS 33°C），
是因为 PowerShell 走的是 **MI API**，不是 WMI COM。

### 3. `root\wmi:MSStorageDriver_ATAPISmartData`

非提权可枚举（2 块 SATA 盘，`VendorSpecific` 362 字节），能解析 SMART 属性 194 拿温度；
但 **NVMe 不出现在这个类里**（系统盘 CN700 就读不到），且解析 SMART 原始属性属于长期维护负担，故未采用。

### 如果以后要做：两条路的实测成本

| | 自己接 MI API | HWiNFO 共享内存 |
|---|---|---|
| 端到端耗时 | 423 ms/轮（3 块盘，CIM cmdlet 含往返延迟） | 读共享内存，微秒级 |
| 提供程序 / 进程 CPU | **65 ms/轮**（WmiPrvSE，约 22 ms/盘） | 0.2~0.5%（单核，2 秒轮询、全量传感器，经验值） |
| 内存 | `WmiPrvSE.exe` WS 24.6 MB（`private` 8 MB） | HWiNFO64 约 25~60 MB（经验值） |
| 权限 | 零提权 | 首次需管理员加载内核驱动 |
| 能拿到 | 只有磁盘温度 | CPU 每核温度/电压/功率、主板、风扇、盘温（完整超集） |
| 稳定性 | 无外部依赖 | 免费版共享内存 **12 小时后自动失效**，需手动再勾选 |
| 开发成本 | 高：MI 是 C ABI，Rust 无绑定，结构体布局要自己对齐，约 300~500 行 unsafe | 中：固定结构体 + 版本校验，约 150 行，但要处理未安装/已失效/版本不匹配三种降级 |

盘温按 30~60 秒采一次即可，折算到单核平均约 **0.15%~0.3%**，资源上两条路都能接受；
真正的差别是 **MI 只能补盘温，HWiNFO 能补 CPU 温度/功率/风扇**。

## 未接入：CPU 温度与功耗

- ACPI 热区：本机 `typeperf "\Thermal Zone Information(*)\High Precision Temperature"` 报"没有有效计数器"，
  `MSAcpi_ThermalZoneTemperature` 报"不支持"——桌面主板一个热区都没有，提权也没有用。
- AMD Zen 的 Tctl/Tdie 在 SMU 寄存器里，读它必须走 MSR/SMN，也就是**必须加载签名内核驱动**。
  这就是 TrafficMonitor 依赖 LibreHardwareMonitor + 驱动、并在 1.86 版把温度功能移出主程序改插件的原因。
- 结论：不引入内核驱动的前提下，这台机器拿不到 CPU 温度。

## 暂不实施的理由与触发条件

1. 盘温对一个桌面监控是"有更好、没有也能用"，而两条路都要付额外复杂度（MI 的 FFI 风险，或 HWiNFO 的外部依赖 + 12 小时坑）。
2. CPU 温度才是这台机器真正的缺口，但唯一可行路径是引入第三方驱动（HWiNFO / LibreHardwareMonitor），
   与"不为读数引入提权/驱动"的既有取舍冲突，需要单独评估。

**重启这个话题的条件**：明确要 CPU 温度或分器件功率。届时优先做"HWiNFO 共享内存优先、MI 兜底盘温"的组合，
采样节奏按 GPU 1 秒、共享内存 2 秒、MI 盘温 30~60 秒。
