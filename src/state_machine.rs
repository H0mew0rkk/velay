//! 核心状态机:自动切换录音设备 + 播放 + 切回。
//!
//! 状态流转:
//!   Idle → 记录当前麦克风 ID → 切到 CABLE Output → 播放 → 切回 → Idle
//!
//! 异常兜底:
//!   - DeviceGuard 用 RAII 保证:即使 panic,切回函数也会在 Drop 时执行。
//!   - 被强杀(进程直接终止)时 Drop 不保证执行,但下次启动时 init() 会检测并自动切回。

use crate::app_policy_config;
use crate::audio_player;
use crate::config::Config;
use crate::pending;
use crate::policy_config::{self, IPolicyConfig};
use std::sync::atomic::{AtomicBool, Ordering};
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
    shutdown_restore();
    // 返回 FALSE:不拦截信号,让系统继续执行默认的终止流程。
    BOOL(0)
}

/// 退出前的统一兜底,GUI 关窗与控制台退出钩子共用。三件事缺一不可:
///
/// 1. 松开自动开麦的语音键——否则游戏内麦克风常开,且没有下次启动的补救;
/// 2. 清除 per-app 覆盖——它是持久化的,残留比"系统默认卡在虚拟声卡"更顽固;
/// 3. 系统默认若卡在虚拟声卡则切回真麦。
///
/// 必须在**非 STA 线程**上调用(内部 COM 走 MTA)。
pub fn shutdown_restore() {
    crate::auto_mic::release_held_key();
    restore_per_app_now();
    if restore_default_mic_if_on_cable() {
        eprintln!("🔙 退出前已将默认录音设备切回真麦克风。");
    }
}

/// 清除当前尚未恢复的 per-app 覆盖(依 [`pending`] 标记,而非当前配置——
/// 用户可能在播放后改过 `target_apps`,要清的是**实际设过覆盖的那些程序**)。
pub fn restore_per_app_now() {
    let apps = pending::pending_apps();
    if !apps.is_empty() {
        per_app_restore(&apps);
    }
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
    let mut original_id = get_default_recording_device_id()
        .map_err(|e| format!("无法获取当前默认录音设备: {}", e))?;

    // 当前默认**已经是**虚拟声卡(上次异常退出没恢复干净,或启动自检因缺少真麦
    // 记录而没能恢复)。此时绝不能把它当成"原设备"记下来——否则播放结束时会"切回"
    // 虚拟声卡,麦克风就永久卡死了。先按配置恢复真麦,再走正常流程。
    if is_cable_device(&original_id) {
        let saved = Config::load().real_mic_device_id;
        let Some(saved) = saved else {
            return Err(
                "默认录音设备卡在虚拟声卡,且配置里没有记录真麦克风。\
                 请先在设置的「设备」里指定真麦克风。"
                    .to_string(),
            );
        };
        if is_cable_device(&saved) {
            return Err(
                "配置里记录的「真麦克风」本身就是虚拟声卡,请在设置的「设备」里改成真实麦克风。"
                    .to_string(),
            );
        }
        eprintln!("⚠ 默认录音设备卡在虚拟声卡,先恢复真麦克风再播放。");
        unsafe { switch_to_device(&saved) };
        original_id = get_default_recording_device_id()
            .map_err(|e| format!("切换后无法获取默认录音设备: {}", e))?;
        if is_cable_device(&original_id) {
            return Err("无法把默认录音设备切回真麦克风(切换未生效)。".to_string());
        }
    }

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
/// 若配置里设置了 `cable_capture_override`,优先采用用户指定的设备 ID。
fn find_cable_recording_device_id() -> Result<String, String> {
    // 用户在 GUI 里手动指定了虚拟声卡录音端 → 直接采用,不再按名称猜测。
    if let Some(id) = Config::load().cable_capture_override {
        if !id.is_empty() {
            return Ok(id);
        }
    }
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

// ═══════════════════════════════════════════════════════════════
// per-app 路由(阶段七):只切换选中程序的录音设备,系统默认不动
// ═══════════════════════════════════════════════════════════════

/// 当前是否有一次 per-app 播放正在进行(覆盖已设、尚未恢复)。
/// 守望线程据此避让:正在播放时的覆盖是**故意**设的,不能去清。
static PER_APP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 收集所有目标程序的 PID(一个程序可能有多个进程)。
fn collect_target_pids(target_apps: &[String]) -> Vec<u32> {
    target_apps
        .iter()
        .flat_map(|app| app_policy_config::find_process_ids(app))
        .collect()
}

/// 把所有目标程序的默认录音设备切到 CABLE Output,返回**成功的进程数**。
///
/// 注意:一个应用常有多个辅助进程,只有真正持有音频会话的主进程能设置成功,
/// 其余会返回 `E_INVALIDARG`——这是**正常现象,不是错误**,所以不逐个报错。
/// 只要至少有一个进程成功,切换就是有效的。
pub fn per_app_to_cable(target_apps: &[String], cable_id: &str) -> usize {
    let pids = collect_target_pids(target_apps);
    if pids.is_empty() {
        return 0; // 目标程序没运行,不算错误
    }

    // 先落盘标记再设覆盖:两者之间崩溃只会导致下次多清一遍(清除幂等),
    // 反过来则会漏清,目标程序永久卡在虚拟声卡。
    pending::mark(target_apps);
    PER_APP_ACTIVE.store(true, Ordering::SeqCst);

    let ok = app_policy_config::set_apps_default_capture(&pids, Some(cable_id));
    if ok == 0 {
        // 一个进程都没设成功 = 没有产生任何覆盖,撤销标记,别留下假的待恢复项。
        PER_APP_ACTIVE.store(false, Ordering::SeqCst);
        pending::clear();
        eprintln!(
            "⚠ 目标程序({})的 {} 个进程都无法切换录音设备。",
            target_apps.join("、"),
            pids.len()
        );
    }
    ok
}

/// 清除所有目标程序的覆盖 → 它们恢复「跟随系统默认」,即真麦克风
/// (per-app 模式下我们从不改系统默认,所以清除即恢复)。
///
/// **目标程序已经退出时清不掉**:这个接口靠活着的 PID 定位应用,进程没了就无从下手,
/// 而覆盖本身是持久化的、仍然留在系统里。这种情况下保留 [`pending`] 标记,
/// 交给启动自检 / 守望线程在它下次启动时清除。
pub fn per_app_restore(target_apps: &[String]) {
    PER_APP_ACTIVE.store(false, Ordering::SeqCst);

    let pids = collect_target_pids(target_apps);
    if pids.is_empty() {
        return; // 清不掉,保留标记
    }
    app_policy_config::set_apps_default_capture(&pids, None);
    pending::clear();
}

/// per-app 版的 RAII 守卫:drop 时清除目标程序的覆盖。
/// 与 [`DeviceGuard`] 同样的用意——panic / 提前 return 也能恢复。
pub struct AppRouteGuard {
    targets: Vec<String>,
}

impl AppRouteGuard {
    pub fn new(targets: Vec<String>) -> Self {
        AppRouteGuard { targets }
    }
}

impl Drop for AppRouteGuard {
    fn drop(&mut self) {
        per_app_restore(&self.targets);
    }
}

/// 启动兜底:per-app 覆盖是**持久化**的(按程序身份保存,重启不消失),
/// 若上次被强杀,目标程序会一直卡在 CABLE(进游戏说话没人听见)。
///
/// 依据 [`pending`] 标记而不是当前配置——标记记的是「上次实际设给了谁」,
/// 用户事后改过 `target_apps` 也不会漏掉旧目标。
///
/// 目标程序此刻没运行时**清不掉**(接口要活 PID),这正是残留最容易发生的场景:
/// 强杀 → 游戏也关了 → 下次先开本工具。此时启动一个守望线程,等目标程序出现再清。
pub fn per_app_startup_cleanup() {
    let apps = pending::pending_apps();
    if apps.is_empty() {
        return; // 上次是干净退出的
    }

    println!(
        "⚠ 检测到上次未正常恢复:{} 的录音设备可能仍被覆盖为虚拟声卡,正在清理...",
        apps.join("、")
    );

    let pids = collect_target_pids(&apps);
    if !pids.is_empty() {
        app_policy_config::set_apps_default_capture(&pids, None);
        pending::clear();
        println!("  已清理。");
        return;
    }

    println!("  目标程序当前没有运行,无法立即清理(该接口需要进程 PID 才能定位应用)。");
    println!("  已转入后台守望:目标程序一启动就自动清除,期间不必做任何事。");
    spawn_pending_watcher(apps);
}

/// 守望线程:等目标程序出现,一出现就清掉它残留的覆盖,然后退出。
///
/// 三个退出/避让条件:
/// - 标记已被别处清掉(如用户点了「紧急恢复」)→ 无事可做,退出;
/// - 正有一次 per-app 播放在进行 → 那个覆盖是**故意**设的,别去清,跳过本轮;
/// - 成功清除 → 退出。
fn spawn_pending_watcher(apps: Vec<String>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));

            if pending::pending_apps().is_empty() {
                return;
            }
            if PER_APP_ACTIVE.load(Ordering::SeqCst) {
                continue;
            }
            let pids = collect_target_pids(&apps);
            if pids.is_empty() {
                continue;
            }
            // 枚举进程要花几毫秒,这期间可能刚好开始了一次播放。清除是"恢复"方向的
            // 操作,误清只会让那次播放的路由失效(游戏听不到音效),不会卡死设备,
            // 但还是再确认一次,把窗口压到最小。
            if PER_APP_ACTIVE.load(Ordering::SeqCst) {
                continue;
            }
            app_policy_config::set_apps_default_capture(&pids, None);
            pending::clear();
            eprintln!(
                "✅ {} 已启动,已自动清除其残留的录音设备覆盖。",
                apps.join("、")
            );
            return;
        }
    });
}

/// 是否还有未清理的 per-app 残留(供 GUI 启动时提示用户)。
pub fn pending_per_app_apps() -> Vec<String> {
    pending::pending_apps()
}

/// 丢弃待恢复标记。仅在覆盖确已被清除时调用(如 GUI 的「紧急恢复」清空了所有覆盖)。
pub fn clear_pending_per_app() {
    pending::clear();
}

// ═══════════════════════════════════════════════════════════════
// 供 GUI / 控制器使用的公开封装
// ═══════════════════════════════════════════════════════════════

/// 查找虚拟声卡录音端(CABLE Output)的设备 ID(GUI/控制器用,暂停恢复时切回)。
pub fn find_cable_capture_id() -> Result<String, String> {
    find_cable_recording_device_id()
}

/// 读取「当前进程视角下」的默认录音设备 ID。注意:若本进程被设置了 per-app 覆盖,
/// 这里返回的就是覆盖后的设备——阶段七的验证正是利用这一点。
pub fn current_default_recording_id() -> Option<String> {
    get_default_recording_device_id().ok()
}

/// 把指定设备设为系统默认录音设备(安全封装,内部走三个角色)。
/// 供控制器在「暂停→切回真麦、恢复→切到 CABLE」时调用。
pub fn set_default_recording(device_id: &str) {
    unsafe { switch_to_device(device_id) };
}

/// 枚举所有活动录音设备,返回 `(设备 ID, 友好名称)` 列表(GUI 下拉框用)。
pub fn list_capture_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).ok().is_err() {
            return out;
        }
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return out;
        };
        let Ok(collection) = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        let Ok(count) = collection.GetCount() else {
            return out;
        };
        for i in 0..count {
            let Ok(device) = collection.Item(i) else { continue };
            let Ok(pwstr) = device.GetId() else { continue };
            let Ok(id) = pwstr.to_string() else { continue };
            let name = device
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|props| props.GetValue(&PKEY_Device_FriendlyName).ok())
                .map(|v| v.to_string())
                .unwrap_or_else(|| id.clone());
            out.push((id, name));
        }
    }
    out
}

/// 判断一个设备 ID 是否指向虚拟声卡的录音端(默认是 VB-Cable 的 CABLE Output)。
///
/// 除了按名称匹配 "cable",还认用户在 GUI 里手动指定的 `cable_capture_override`——
/// 否则用户指定了一个名字里不含 "cable" 的虚拟设备时,启动自检和退出恢复都会
/// 认不出"卡在虚拟声卡"这个状态,兜底逻辑全部失效。
fn is_cable_device(device_id: &str) -> bool {
    if let Some(id) = Config::load().cable_capture_override {
        if id == device_id {
            return true;
        }
    }
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
