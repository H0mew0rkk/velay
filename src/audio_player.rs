//! 音频播放模块:把 mp3/wav 播放到指定的输出设备(CABLE Input)。
//!
//! 用 rodio 做解码和播放,用其内部重导出的 cpal 做设备枚举。

use rodio::cpal::traits::HostTrait;
use rodio::{Decoder, DeviceSinkBuilder, DeviceTrait, MixerDeviceSink, Player};
use std::fs::File;
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 在输出设备列表里找到 VB-Cable 虚拟播放设备(CABLE Input)。
pub fn find_cable_output_device() -> Result<rodio::Device, String> {
    let host = rodio::cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| format!("无法枚举输出设备: {}", e))?;

    for device in devices {
        let name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_default();
        if name.to_lowercase().contains("vb-audio")
            || name.to_lowercase().contains("cable")
        {
            println!("找到 CABLE 输出设备: {}", name);
            return Ok(device);
        }
    }

    Err("未找到 VB-Cable 输出设备(CABLE Input),请确认已安装 VB-Cable。".to_string())
}

/// 列出系统所有输出设备(调试用)。
pub fn list_output_devices() {
    let host = rodio::cpal::default_host();
    let devices = match host.output_devices() {
        Ok(d) => d,
        Err(e) => {
            println!("无法枚举输出设备: {}", e);
            return;
        }
    };

    println!("输出设备列表:");
    for (i, device) in devices.enumerate() {
        let desc = device.description();
        let name = desc.as_ref().map(|d| d.name()).unwrap_or("未知设备");
        let manufacturer = desc
            .as_ref()
            .map(|d| d.manufacturer().unwrap_or("-"))
            .unwrap_or("-");
        println!("  [{}] {}  (制造商: {})", i, name, manufacturer);
    }
}

/// CLI 模式:播放并阻塞等待用户按 Enter 停止。
pub fn play_to_cable(file_path: &str) -> Result<(), String> {
    let (handle, player) = setup_playback(file_path, 1.0)?;

    println!("按 Enter 停止播放...");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();

    player.stop();
    drop(handle);

    println!("播放已停止。");
    Ok(())
}

/// 守护进程模式:播放并周期性检查 stop_flag,外部设置为 true 时停止。
/// `stop_flag` 初始应为 `false`,调用方在需要停止时设为 `true`。
pub fn play_to_cable_interruptible(
    file_path: &str,
    stop_flag: Arc<AtomicBool>,
) -> Result<(), String> {
    let (_handle, player) = setup_playback(file_path, 1.0)?;

    println!("▶ {}", file_path);

    // 循环等待:音频自然播完 或 外部 stop_flag 被触发
    while !stop_flag.load(Ordering::Relaxed) && !player.empty() {
        std::thread::sleep(Duration::from_millis(200));
    }

    if stop_flag.load(Ordering::Relaxed) {
        player.stop();
        println!("■ 已停止(外部信号)");
    }

    // _handle 在此 drop,释放设备
    Ok(())
}

/// GUI/控制器模式:打开到 CABLE Input 的播放器但**不阻塞**,由调用方持有
/// `(MixerDeviceSink, Player)` 并自行控制暂停/恢复/停止/音量。
/// `volume` 在追加音频前设好,避免起播瞬间以满音量播放。
pub fn open_player(file_path: &str, volume: f32) -> Result<(MixerDeviceSink, Player), String> {
    setup_playback(file_path, volume)
}

/// 本地监听:再打开一路播放器到**本机默认输出设备**(耳机/音箱),让用户自己也能
/// 听到正在播的音效。与主路(CABLE Input)相互独立、各自解码一份。
///
/// 若默认输出设备本身就是 CABLE(用户把系统默认输出设成了虚拟声卡),则返回错误
/// 让调用方跳过——否则会往同一个设备播两份。监听是锦上添花,调用方应容忍其失败。
pub fn open_monitor_player(
    file_path: &str,
    volume: f32,
) -> Result<(MixerDeviceSink, Player), String> {
    let host = rodio::cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "没有默认输出设备,跳过本地监听".to_string())?;

    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_default()
        .to_lowercase();
    if name.contains("cable") || name.contains("vb-audio") {
        return Err("默认输出设备是 CABLE,跳过本地监听以免重复播放".to_string());
    }

    open_on_device(device, file_path, volume)
}

// ═══════════════════════════════════════════════════════════════
// 内部函数
// ═══════════════════════════════════════════════════════════════

/// 核心:创建到 CABLE Input 的输出流、解码音频、追加到 Player。
/// 返回 `(MixerDeviceSink, Player)`,调用方负责生命周期管理。
fn setup_playback(file_path: &str, volume: f32) -> Result<(MixerDeviceSink, Player), String> {
    let device = find_cable_output_device()?;
    open_on_device(device, file_path, volume)
}

/// 在指定输出设备上打开一路播放器,解码 `file_path` 并按 `volume` 起播。
/// 主路(CABLE Input)和本地监听路(默认输出)共用这段逻辑。
fn open_on_device(
    device: rodio::Device,
    file_path: &str,
    volume: f32,
) -> Result<(MixerDeviceSink, Player), String> {
    let mut handle = DeviceSinkBuilder::from_device(device)
        .map_err(|e| format!("无法绑定输出设备: {}", e))?
        .open_stream()
        .map_err(|e| format!("无法打开音频流: {}", e))?;

    // 我们是有意在停止/播完时 drop 掉 sink 的,rodio 那句提醒属于噪音,关掉。
    handle.log_on_drop(false);

    let player = Player::connect_new(handle.mixer());
    player.set_volume(volume);

    let file = BufReader::new(
        File::open(file_path).map_err(|e| format!("无法打开文件 {}: {}", file_path, e))?,
    );

    let source = Decoder::try_from(file)
        .map_err(|e| format!("无法解码音频 {}: {}", file_path, e))?;

    player.append(source);

    Ok((handle, player))
}
