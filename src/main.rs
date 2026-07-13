mod audio_player;
mod config;
mod hotkey_daemon;
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
    // 每次启动都检查:如果上次异常退出导致麦克风卡在 CABLE Output,自动切回
    state_machine::init();
    // 注册退出钩子(Ctrl+C / 关闭窗口 / 注销 / 关机):终止前把录音设备切回真麦克风
    state_machine::install_exit_handler();

    let args: Vec<String> = std::env::args().collect();
    let Some(command) = args.get(1) else {
        print_usage();
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
        // 默认:把参数当关键词,切换默认录音设备(保持向后兼容)
        keyword => {
            switch_recording_device(keyword)?;
        }
    }

    Ok(())
}

fn print_usage() {
    println!("soundpad — Windows 语音音效播放器(开发中)");
    println!();
    println!("用法:");
    println!("  soundpad daemon         启动热键守护进程");
    println!("  soundpad play <文件>    播放音频(自动切换录音设备)");
    println!("  soundpad devices        列出所有输出设备");
    println!("  soundpad rec            列出所有录音设备");
    println!("  soundpad <关键词>       切换默认录音设备(如: cable / INZONE)");
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
