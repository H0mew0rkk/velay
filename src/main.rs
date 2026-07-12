mod policy_config;

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
    // 从命令行参数取"要切换到的设备名关键词",例如:
    //   cargo run -- cable         → 切到名字里含 "cable" 的设备(不区分大小写)
    //   cargo run -- "INZONE"      → 切到名字里含 "INZONE" 的设备
    let args: Vec<String> = std::env::args().collect();
    let Some(keyword) = args.get(1) else {
        println!("用法: soundpad <设备名关键词>");
        println!("例如: soundpad cable   (切换默认录音设备到 CABLE Output)");
        return Ok(());
    };
    let keyword_lower = keyword.to_lowercase();

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;

        // 找第一个名字里包含关键词的录音设备
        let mut found: Option<(String, String)> = None;
        for i in 0..count {
            let device = collection.Item(i)?;
            let id = device.GetId()?.to_string()?;
            let props = device.OpenPropertyStore(STGM_READ)?;
            let name = props.GetValue(&PKEY_Device_FriendlyName)?.to_string();

            if name.to_lowercase().contains(&keyword_lower) {
                found = Some((id, name));
                break;
            }
        }

        let Some((device_id, device_name)) = found else {
            println!("没找到名字包含 \"{keyword}\" 的录音设备。");
            return Ok(());
        };

        println!("匹配到设备: {device_name}");
        println!("正在切换默认录音设备...");

        // device_id 是 Rust String,COM 接口要的是"以 \0 结尾的 UTF-16 宽字符串"指针(PCWSTR)。
        // 这里手动把 String 编码成 UTF-16 并补上结尾的 0。
        let wide_id: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        let device_id_pcwstr = PCWSTR::from_raw(wide_id.as_ptr());

        let policy_config: IPolicyConfig =
            CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;

        // 三种角色都切一遍,系统提示音/媒体播放/通话软件的默认设备才会完全一致
        for role in [eConsole, eMultimedia, eCommunications] {
            policy_config.SetDefaultEndpoint(device_id_pcwstr, role)?;
        }

        println!("切换完成,请去 Windows 声音设置里确认。");
    }

    Ok(())
}