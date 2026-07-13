//! 核心状态机:自动切换录音设备 + 播放 + 切回。
//!
//! 状态流转:
//!   Idle → 记录当前麦克风 ID → 切到 CABLE Output → 播放 → 切回 → Idle
//!
//! 异常兜底:
//!   - DeviceGuard 用 RAII 保证:即使 panic,切回函数也会在 Drop 时执行。
//!   - 被强杀(进程直接终止)时 Drop 不保证执行,但下次启动时 init() 会检测并自动切回。

use crate::audio_player;
use crate::config::Config;
use crate::policy_config::{self, IPolicyConfig};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications,
    eConsole, eMultimedia,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CoCreateInstance, CoInitializeEx, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::core::{BOOL, PCWSTR};

// ═══════════════════════════════════════════════════════════════
// 公开 API
// ═══════════════════════════════════════════════════════════════

/// 程序启动时调用:检查默认录音设备是否卡在 CABLE Output,是则切回真麦克风。
pub fn init() {
    let Ok(current_id) = get_default_recording_device_id() else {
        return;
    };

    // 如果不是 CABLE Output,说明上次是正常退出的,不需要清理
    if !is_cable_device(&current_id) {
        return;
    }

    // 卡在 CABLE Output 了——说明上次异常退出,尝试恢复
    println!("⚠ 检测到默认录音设备仍为 CABLE Output(上次可能异常退出),正在恢复...");

    let config = Config::load();
    if let Some(saved_id) = &config.real_mic_device_id {
        println!("  从配置恢复真麦克风: {}",
            config.real_mic_device_name.as_deref().unwrap_or(saved_id));
        unsafe { switch_to_device(saved_id) };
        println!("  已切回。");
    } else {
        println!("  配置里没有记录真麦克风,请手动切回或运行:");
        println!("    soundpad <你的麦克风关键词>");
        println!("  然后重新运行程序。");
    }
}

/// 注册控制台退出钩子:Ctrl+C / 关闭窗口 / 注销 / 关机 时,
/// 在进程被终止前把默认录音设备切回真麦克风,避免麦克风"卡"在 CABLE Output。
///
/// 注意:被强杀(任务管理器结束进程 / `TerminateProcess`)时此钩子不会触发,
/// 那种情况仍依赖下次启动的 [`init`] 兜底。
pub fn install_exit_handler() {
    unsafe {
        if SetConsoleCtrlHandler(Some(console_ctrl_handler), true).is_err() {
            eprintln!("⚠ 无法注册退出钩子,异常退出时可能需要手动切回麦克风。");
        }
    }
}

/// 控制台控制事件回调。由系统在一个独立线程里调用,
/// 因此内部的 COM 调用会各自 `CoInitializeEx`,无需在此初始化。
unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> BOOL {
    if restore_default_mic_if_on_cable() {
        eprintln!("🔙 退出前已将默认录音设备切回真麦克风。");
    }
    // 返回 FALSE:不拦截信号,让系统继续执行默认的终止流程。
    BOOL(0)
}

/// 静默恢复:若当前默认录音设备卡在 CABLE Output,依配置切回真麦克风。
/// 返回是否真的执行了切回。供退出钩子复用。
fn restore_default_mic_if_on_cable() -> bool {
    let Ok(current_id) = get_default_recording_device_id() else {
        return false;
    };
    if !is_cable_device(&current_id) {
        return false;
    }
    let config = Config::load();
    if let Some(saved_id) = &config.real_mic_device_id {
        unsafe { switch_to_device(saved_id) };
        true
    } else {
        false
    }
}

/// 播放音频并自动切换录音设备。
/// 流程图:
///   1. 记录当前默认录音设备
///   2. 如果尚未配置真麦克风,把当前设备存为真麦克风
///   3. 切默认录音 → CABLE Output
///   4. 播放音频到 CABLE Input(阻塞直到用户按 Enter 或音频结束)
///   5. 切默认录音 → 第 1 步记录的设备
pub fn play_with_auto_switch(file_path: &str) -> Result<(), String> {
    // 1. 记录当前设备
    let original_id = get_default_recording_device_id()
        .map_err(|e| format!("无法获取当前默认录音设备: {}", e))?;
    let original_name = get_device_name(&original_id)
        .unwrap_or_else(|_| original_id.clone());

    println!("当前录音设备: {}", original_name);

    // 2. 如果是 CABLE Output,说明状态不对(上次没切回),先跳过自动切回逻辑
    if is_cable_device(&original_id) {
        println!("⚠ 当前默认录音设备已经是 CABLE Output,将先尝试恢复...");
        let config = Config::load();
        if let Some(saved_id) = &config.real_mic_device_id {
            println!("  从配置恢复真麦克风并重新开始流程。");
            unsafe { switch_to_device(saved_id) };
            // 重新获取
            let real_id = get_default_recording_device_id()
                .map_err(|e| format!("切换后无法获取录音设备: {}", e))?;
            return play_with_auto_switch_inner(file_path, &real_id);
        } else {
            return Err("默认录音设备卡在 CABLE Output 且配置中没有真麦克风信息,请先手动切回。".to_string());
        }
    }

    // 正常路径
    play_with_auto_switch_inner(file_path, &original_id)
}

/// 播放的核心流程(内部实现,不需要管初始化/恢复等边界情况)。
fn play_with_auto_switch_inner(file_path: &str, original_device_id: &str) -> Result<(), String> {
    // 如果还没存过真麦克风,把当前设备存下来(首次使用时自动记录)
    let mut config = Config::load();
    if config.real_mic_device_id.is_none() {
        config.real_mic_device_id = Some(original_device_id.to_string());
        config.real_mic_device_name = Some(get_device_name(original_device_id).unwrap_or_default());
        config.save();
        println!("📝 已记录真麦克风: {} (ID 已保存到 config.json)",
            config.real_mic_device_name.as_deref().unwrap_or("-"));
    }

    // 找 CABLE Output 的设备 ID
    let cable_id = find_cable_recording_device_id()
        .map_err(|e| format!("无法找到 CABLE Output: {}", e))?;

    // 切到 CABLE Output
    println!("🔄 切换录音设备: {} → CABLE Output",
        config.real_mic_device_name.as_deref().unwrap_or("当前设备"));
    unsafe { switch_to_device(&cable_id) };

    // 创建 DeviceGuard:这个变量存在期间如果发生 panic/提前 return,
    // Drop 实现会保证切回原设备
    let _guard = DeviceGuard {
        original_id: original_device_id.to_string(),
    };

    // 播放(阻塞)
    println!("▶ 开始播放...");
    audio_player::play_to_cable(file_path)
        .map_err(|e| format!("播放失败: {}", e))?;

    // guard 在此 drop,自动切回——但为了一致性,显式切回也可。
    // DeviceGuard 的 Drop 会执行 switch_to_device(&self.original_id)
    println!("✅ 播放流程结束。");

    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// DeviceGuard:RAII 守卫,保证切回
// ═══════════════════════════════════════════════════════════════

pub struct DeviceGuard {
    original_id: String,
}

impl DeviceGuard {
    pub fn new(original_id: String) -> Self {
        DeviceGuard { original_id }
    }
}

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        println!("🔙 切回录音设备...");
        unsafe { switch_to_device(&self.original_id) };
        println!("   已切回。");
    }
}

// ═══════════════════════════════════════════════════════════════
// 供守护进程使用的播放准备/清理 API
// ═══════════════════════════════════════════════════════════════

/// 播放前准备:记录当前麦克风并切到 CABLE Output。
/// 返回原始设备 ID,调用方用 DeviceGuard 或手动切回。
pub fn prepare_playback() -> Result<String, String> {
    let original_id = get_default_recording_device_id()
        .map_err(|e| format!("无法获取当前默认录音设备: {}", e))?;

    // 首次使用时自动记录真麦克风
    let mut config = Config::load();
    if config.real_mic_device_id.is_none() && !is_cable_device(&original_id) {
        config.real_mic_device_id = Some(original_id.clone());
        config.real_mic_device_name =
            Some(get_device_name(&original_id).unwrap_or_default());
        config.save();
        println!(
            "📝 已记录真麦克风: {}",
            config.real_mic_device_name.as_deref().unwrap_or("-")
        );
    }

    let cable_id = find_cable_recording_device_id()
        .map_err(|e| format!("无法找到 CABLE Output: {}", e))?;

    println!("🔄 切换录音设备 → CABLE Output");
    unsafe { switch_to_device(&cable_id) };

    Ok(original_id)
}

// ═══════════════════════════════════════════════════════════════
// 底层工具函数
// ═══════════════════════════════════════════════════════════════

/// 获取当前系统默认录音设备的 endpoint ID 字符串。
fn get_default_recording_device_id() -> Result<String, String> {
    unsafe {
        // 每次调用都是一个独立的 COM 调用,所以需要自己 CoInitialize
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|_| "COM 初始化失败".to_string())?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|_| "无法创建 MMDeviceEnumerator".to_string())?;

        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|_| "无法获取默认录音设备".to_string())?;

        device
            .GetId()
            .map_err(|_| "无法读取设备 ID".to_string())?
            .to_string()
            .map_err(|_| "设备 ID 编码错误".to_string())
    }
}

/// 查找 CABLE Output 录音设备的 endpoint ID。
fn find_cable_recording_device_id() -> Result<String, String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|_| "COM 初始化失败".to_string())?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|_| "无法创建 MMDeviceEnumerator".to_string())?;
        let collection = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|_| "无法枚举录音设备".to_string())?;
        let count = collection
            .GetCount()
            .map_err(|_| "无法获取设备数量".to_string())?;

        for i in 0..count {
            let device = collection
                .Item(i)
                .map_err(|_| "无法获取设备".to_string())?;
            let props = device
                .OpenPropertyStore(STGM_READ)
                .map_err(|_| "无法打开属性".to_string())?;
            let name = props
                .GetValue(&PKEY_Device_FriendlyName)
                .ok()
                .map(|v| v.to_string())
                .unwrap_or_default();

            if name.to_lowercase().contains("cable") {
                return device
                    .GetId()
                    .map_err(|_| "无法读取设备 ID".to_string())?
                    .to_string()
                    .map_err(|_| "设备 ID 编码错误".to_string());
            }
        }

        Err("未找到 CABLE Output 录音设备,请确认 VB-Cable 已安装。".to_string())
    }
}

/// 通过设备名称读取其友好名称(用于日志)。
fn get_device_name(device_id: &str) -> Result<String, String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|_| "COM 初始化失败".to_string())?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|_| "无法创建 MMDeviceEnumerator".to_string())?;
        let collection = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|_| "无法枚举录音设备".to_string())?;
        let count = collection
            .GetCount()
            .map_err(|_| "无法获取设备数量".to_string())?;

        for i in 0..count {
            let device = collection
                .Item(i)
                .map_err(|_| "无法获取设备".to_string())?;
            let id = device
                .GetId()
                .map_err(|_| "无法读取 ID".to_string())?
                .to_string()
                .map_err(|_| "ID 编码错误".to_string())?;
            if id == device_id {
                let props = device
                    .OpenPropertyStore(STGM_READ)
                    .map_err(|_| "无法打开属性".to_string())?;
                return Ok(props
                    .GetValue(&PKEY_Device_FriendlyName)
                    .ok()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| device_id.to_string()));
            }
        }

        Ok(device_id.to_string())
    }
}

/// 判断一个设备 ID 是否指向 CABLE Output(VB-Cable 虚拟录音设备)。
fn is_cable_device(device_id: &str) -> bool {
    get_device_name(device_id)
        .map(|n| n.to_lowercase().contains("cable"))
        .unwrap_or(false)
}

/// 底层切换函数:把 device_id 对应的设备设为默认录音设备(三个角色都切)。
unsafe fn switch_to_device(device_id: &str) {
    let wide: Vec<u16> = device_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let pcwstr = PCWSTR::from_raw(wide.as_ptr());

    // Rust 2024 edition:unsafe fn 内部仍需要显式 unsafe 块
    unsafe {
        match CoCreateInstance::<_, IPolicyConfig>(
            &policy_config::CLSID_POLICY_CONFIG_CLIENT,
            None,
            CLSCTX_ALL,
        ) {
            Ok(policy) => {
                for role in [eConsole, eMultimedia, eCommunications] {
                    if let Err(e) = policy.SetDefaultEndpoint(pcwstr, role) {
                        // 例如 E_NOINTERFACE(不同 Windows 版本 vtable 差异)会在此暴露,
                        // 而不是静默失败导致用户"说话没人听见"却查不到原因。
                        eprintln!("⚠ 切换默认录音设备失败(role={}): {}", role.0, e);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "⚠ 无法创建 IPolicyConfig(可能是 Windows 版本 ABI 差异 / VB-Cable 未装): {}",
                    e
                );
            }
        }
    }
}
