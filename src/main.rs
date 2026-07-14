mod app_policy_config;
mod app_state;
mod audio_player;
mod auto_mic;
mod config;
mod gui;
mod hotkey_daemon;
mod hotkeys;
mod pending;
mod policy_config;
mod state_machine;

use policy_config::{CLSID_POLICY_CONFIG_CLIENT, IPolicyConfig};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications,
    eConsole, eMultimedia,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CoCreateInstance, CoInitializeEx, COINIT_MULTITHREADED, STGM_READ,
};
use windows::core::{PCWSTR, Result};

fn main() -> Result<()> {
    // 每次启动都检查:如果上次异常退出导致麦克风卡在 CABLE Output,自动切回。
    // 注意:必须在**独立线程**里做——init() 会 CoInitializeEx(MTA),而 GUI 模式下
    // winit 需要主线程是 STA(OleInitialize,用于拖放),若主线程被 MTA 初始化会 panic。
    // 同一线程里顺带做 per-app 兜底:per-app 覆盖是持久化的(按程序身份保存、重启不消失),
    // 上次若被强杀会残留,导致目标程序一直卡在虚拟麦克风(游戏里说话没人听见)。
    let _ = std::thread::spawn(|| {
        state_machine::init();
        state_machine::per_app_startup_cleanup();
    })
    .join();
    // 注册退出钩子(Ctrl+C / 关闭窗口 / 注销 / 关机):终止前把录音设备切回真麦克风
    state_machine::install_exit_handler();

    let args: Vec<String> = std::env::args().collect();
    let Some(command) = args.get(1) else {
        // 无子命令 → 启动图形界面(阶段五)。
        if let Err(e) = gui::run_gui() {
            eprintln!("GUI 启动失败: {e}");
        }
        return Ok(());
    };

    match command.as_str() {
        "play" => {
            let Some(file_path) = args.get(2) else {
                println!("用法: soundpad play <音频文件路径>");
                println!("例如: soundpad play C:\\music\\hello.mp3");
                return Ok(());
            };
            state_machine::play_with_auto_switch(file_path)
                .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e))?;
        }
        "devices" | "list" => {
            println!("=== 输出设备 ===");
            audio_player::list_output_devices();
        }
        "rec" | "recording" => {
            list_recording_devices()?;
        }
        "daemon" => {
            hotkey_daemon::run();
        }
        "gui" => {
            if let Err(e) = gui::run_gui() {
                eprintln!("GUI 启动失败: {e}");
            }
        }
        "help" | "-h" | "--help" => {
            print_usage();
        }
        // 阶段七开发用:验证 IAudioPolicyConfig(per-app 路由)在本机能否绑定与调用。
        // 只对本程序自己的进程设置再清除,不影响任何真实应用。
        // 开发用:走与 GUI 完全相同的控制器路径播一首,验证设备切换 + 播放 + 本地监听。
        "controller-test" => {
            let cfg = config::Config::load();
            let Some(sound) = cfg.sounds.first().cloned() else {
                println!("config.json 里没有音效,先用 GUI 添加一个。");
                return Ok(());
            };
            println!("路由模式: {:?}", cfg.routing_mode);
            println!("本地监听: {}", cfg.local_monitor);
            println!("播放: {}", sound.name);

            let shared = std::sync::Arc::new(std::sync::Mutex::new(app_state::Shared::new(&cfg)));
            let tx = app_state::spawn_controller(std::sync::Arc::clone(&shared));
            let _ = tx.send(app_state::Command::Play {
                path: sound.path.clone(),
                name: sound.name.clone(),
            });

            std::thread::sleep(std::time::Duration::from_secs(3));
            {
                let s = shared.lock().unwrap();
                println!("\n3 秒后 → 状态: {:?}", s.status);
                match &s.error {
                    Some(e) => println!("           错误: {e}"),
                    None => println!("           错误: 无 ✅"),
                }
            }
            let _ = tx.send(app_state::Command::Stop);
            std::thread::sleep(std::time::Duration::from_millis(800));
            let s = shared.lock().unwrap();
            println!("停止后 → 状态: {:?}", s.status);
        }
        // 列出有窗口的程序(GUI 目标程序选择器用的就是这个列表)
        "apps" => {
            let apps = app_policy_config::list_windowed_apps();
            println!("有窗口的程序({} 个):", apps.len());
            for (exe, title) in &apps {
                println!("  {title}  —  {exe}");
            }
        }
        // 列出全部运行进程名(兜底,用来确认目标程序的准确 exe 名)
        "procs" => {
            let names = app_policy_config::list_process_names();
            let filter = args.get(2).map(|s| s.to_lowercase());
            println!("运行中的进程({} 个):", names.len());
            for n in &names {
                if let Some(f) = &filter {
                    if !n.to_lowercase().contains(f) {
                        continue;
                    }
                }
                println!("  {n}");
            }
        }
        // 诊断:对指定进程逐角色尝试设置,定位哪个角色失败
        "approute-diag" => {
            let Some(name) = args.get(2) else {
                println!("用法: soundpad approute-diag <进程名>   例如 Weixin.exe");
                return Ok(());
            };
            match state_machine::find_cable_capture_id() {
                Ok(cable_id) => app_policy_config::diag(name, &cable_id),
                Err(e) => println!("找不到 CABLE Output 录音设备:{e}"),
            }
        }
        "approute-selftest" => match state_machine::find_cable_capture_id() {
            Ok(cable_id) => app_policy_config::selftest(&cable_id),
            Err(e) => println!("找不到 CABLE Output 录音设备:{e}"),
        },
        // 默认:把参数当关键词,切换默认录音设备(保持向后兼容)
        keyword => {
            switch_recording_device(keyword)?;
        }
    }

    Ok(())
}

fn print_usage() {
    println!("soundpad — Windows 语音音效播放器");
    println!();
    println!("用法:");
    println!("  soundpad                无参数 → 启动图形界面(GUI)");
    println!("  soundpad gui            显式启动图形界面");
    println!("  soundpad daemon         启动热键守护进程(旧 per-key 模式)");
    println!("  soundpad play <文件>    播放音频(自动切换录音设备)");
    println!("  soundpad devices        列出所有输出设备");
    println!("  soundpad rec            列出所有录音设备");
    println!("  soundpad <关键词>       切换默认录音设备(如: cable / INZONE)");
    println!();
    println!("诊断:");
    println!("  soundpad apps                  列出有窗口的程序(选目标程序用)");
    println!("  soundpad procs [关键词]        列出运行进程名");
    println!("  soundpad approute-selftest     自测 per-app 路由接口是否可用");
    println!("  soundpad approute-diag <进程>  逐进程/逐角色诊断 per-app 切换");
    println!("  soundpad controller-test       走 GUI 同款控制器路径试播一次");
}

/// 列出所有录音设备(用于调试/确认设备名)。
fn list_recording_devices() -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;

        println!("录音设备列表:");
        for i in 0..count {
            let device = collection.Item(i)?;
            let props = device.OpenPropertyStore(STGM_READ)?;
            let name = props
                .GetValue(&PKEY_Device_FriendlyName)?
                .to_string();
            let id = device.GetId()?.to_string()?;
            println!("  [{}] {}  (id = {})", i, name, id);
        }
    }
    Ok(())
}

/// 按名称关键词找到录音设备,并将其切换为系统默认。
fn switch_recording_device(keyword: &str) -> Result<()> {
    let keyword_lower = keyword.to_lowercase();

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;

        let mut found: Option<(String, String)> = None;
        for i in 0..count {
            let device = collection.Item(i)?;
            let id = device.GetId()?.to_string()?;
            let props = device.OpenPropertyStore(STGM_READ)?;
            let name = props
                .GetValue(&PKEY_Device_FriendlyName)?
                .to_string();

            if name.to_lowercase().contains(&keyword_lower) {
                found = Some((id, name));
                break;
            }
        }

        let Some((device_id, device_name)) = found else {
            println!("没找到名字包含 \"{keyword}\" 的录音设备。");
            println!("可用录音设备:");
            for i in 0..count {
                let device = collection.Item(i)?;
                let props = device.OpenPropertyStore(STGM_READ)?;
                let name = props
                    .GetValue(&PKEY_Device_FriendlyName)?
                    .to_string();
                println!("  [{}] {}", i, name);
            }
            return Ok(());
        };

        println!("匹配到设备: {device_name}");
        println!("正在切换默认录音设备...");

        let wide_id: Vec<u16> = device_id
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let device_id_pcwstr = PCWSTR::from_raw(wide_id.as_ptr());

        let policy_config: IPolicyConfig =
            CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;

        for role in [eConsole, eMultimedia, eCommunications] {
            policy_config.SetDefaultEndpoint(device_id_pcwstr, role)?;
        }

        println!("切换完成,请去 Windows 声音设置里确认。");
    }

    Ok(())
}
