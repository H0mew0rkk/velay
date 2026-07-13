//! 音频播放模块:把 mp3/wav 播放到指定的输出设备(CABLE Input)。
//!
//! 用 rodio 做解码和播放,用其内部重导出的 cpal 做设备枚举。

use rodio::cpal::traits::HostTrait;
use rodio::{Decoder, DeviceSinkBuilder, DeviceTrait, MixerDeviceSink, Player, Source};
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
    let (handle, player) = setup_playback(file_path)?;

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
    let (_handle, player) = setup_playback(file_path)?;

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

// ═══════════════════════════════════════════════════════════════
// 内部函数
// ═══════════════════════════════════════════════════════════════

/// 核心:创建到 CABLE Input 的输出流、解码音频、追加到 Player。
/// 返回 `(MixerDeviceSink, Player)`,调用方负责生命周期管理。
fn setup_playback(file_path: &str) -> Result<(MixerDeviceSink, Player), String> {
    let device = find_cable_output_device()?;

    let handle = DeviceSinkBuilder::from_device(device)
        .map_err(|e| format!("无法绑定输出设备: {}", e))?
        .open_stream()
        .map_err(|e| format!("无法打开音频流: {}", e))?;

    let player = Player::connect_new(handle.mixer());

    let file = BufReader::new(
        File::open(file_path).map_err(|e| format!("无法打开文件 {}: {}", file_path, e))?,
    );

    let source = Decoder::try_from(file)
        .map_err(|e| format!("无法解码音频 {}: {}", file_path, e))?;

    let total_dur = source.total_duration();
    if let Some(dur) = total_dur {
        println!("总时长: {:.1} 秒", dur.as_secs_f32());
    }

    player.append(source);

    Ok((handle, player))
}
