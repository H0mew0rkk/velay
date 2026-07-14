//! 热键守护进程:注册全局热键,按键触发音频播放 + 自动切换录音设备。
//!
//! 主线程跑 win-hotkeys 的事件循环(底层是 WH_KEYBOARD_LL 低级键盘钩子),
//! 每个热键触发时 spawn 一个独立线程走完整的"切设备→播放→切回"流程。
//!
//! 停止键由用户在 config.json 里把某个按键映射为 `__STOP__` 指定(不硬编码某个键):
//! 按下后设置 stop_flag,当前播放线程检测到后停止。

use crate::audio_player;
use crate::config::Config;
use crate::state_machine;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use win_hotkeys::VKey;

/// 当前正在播放时使用的停止标志。
/// 新热键触发时先设置此标志通知旧线程停止,再 spawn 新线程。
struct PlaybackState {
    stop_flag: Arc<AtomicBool>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

pub fn run() {
    // 启动清理(init)与退出钩子的注册统一在 main 里做,这里不再重复。

    let config = Config::load();

    // 如实告知功能边界:daemon 是阶段四的旧模式,后续阶段的功能都长在 GUI 控制器上,
    // 没有回填到这里。用户若以为配置里的设置对 daemon 也生效,会遇到莫名其妙的行为
    // ——尤其是路由模式:他以为只切游戏,实际切的是系统默认(微信也会被牵连)。
    println!("⚠ daemon 是旧的 per-key 模式,只支持「全局」切换系统默认麦克风。");
    println!("  不支持:仅选中程序路由 / 本地监听 / 音量设置——这些请用图形界面");
    println!("  (不带任何子命令直接运行本程序即可)。");
    if config.routing_mode == crate::config::RoutingMode::PerApp {
        println!();
        println!("⚠ 你的配置是「仅选中程序」模式,但 daemon 不支持,仍会切换系统默认麦克风");
        println!("  (播放期间微信等程序也会收到音效)。要用 per-app 路由请改用图形界面。");
    }
    println!();

    if config.hotkeys.is_empty() {
        println!("⚠ config.json 中没有配置热键。");
        println!("  请编辑 config.json,添加 \"hotkeys\" 字段:");
        println!("  {{");
        println!("    \"hotkeys\": {{");
        println!("      \"Numpad1\": \"C:\\\\sounds\\\\hello.mp3\",");
        println!("      \"Numpad2\": \"C:\\\\sounds\\\\laugh.mp3\",");
        println!("      \"Numpad0\": \"__STOP__\"");
        println!("    }}");
        println!("  }}");
        println!();
        println!("  之后重新运行 soundpad daemon 即可。");
        return;
    }

    // 播放状态:同一时间只允许一个音效在播(新触发会先停旧的)
    let state = Arc::new(Mutex::new(PlaybackState {
        stop_flag: Arc::new(AtomicBool::new(false)),
        thread_handle: None,
    }));

    let mut manager = win_hotkeys::HotkeyManager::new();

    // 注册每个热键
    for (key_name, file_path) in &config.hotkeys {
        let vkey = match parse_vkey(key_name) {
            Some(k) => k,
            None => {
                println!("⚠ 忽略未知按键: {}", key_name);
                continue;
            }
        };

        let file_path = file_path.clone();
        let state = Arc::clone(&state);

        // __STOP__ 为停止键,使用用户在 config 里指定的按键(不硬编码 Numpad0)
        if file_path == "__STOP__" {
            let stop_state = Arc::clone(&state);
            manager
                .register_hotkey(vkey, &[], move || {
                    let ps = stop_state.lock().unwrap();
                    ps.stop_flag.store(true, Ordering::SeqCst);
                    println!("⏹ 停止信号已发送");
                })
                .ok();
            println!("  已注册: {} → 停止", key_name);
        } else {
            let fp = file_path.clone();
            let hs = Arc::clone(&state);
            manager
                .register_hotkey(vkey, &[], move || {
                    handle_hotkey(&fp, &hs);
                })
                .ok();
            println!("  已注册: {} → {}", key_name, file_path);
        }
    }

    println!();
    println!("🎤 热键守护进程已启动,按注册的热键触发音效。");
    println!("   关闭此窗口即可退出。");
    println!();

    // 阻塞运行事件循环
    manager.event_loop();
}

/// 热键被按下时调用(在 win-hotkeys 的回调线程中执行)。
fn handle_hotkey(file_path: &str, state: &Arc<Mutex<PlaybackState>>) {
    // 1. 通知旧线程停止,并取出其句柄。注意:join 必须在锁**外**执行——
    //    播放线程结束前不会再回来锁 state(见 play_sound 已改为直接持有 stop_flag),
    //    但仍坚持锁外 join,避免任何回调持锁等待线程、线程等锁的交叉死锁。
    let old_handle = {
        let mut ps = state.lock().unwrap();
        ps.stop_flag.store(true, Ordering::SeqCst);
        ps.thread_handle.take()
    };
    if let Some(handle) = old_handle {
        let _ = handle.join();
    }

    // 2. 新建停止标志并先写回 state:这样在新线程 spawn 期间按下停止键也能命中它。
    let stop_flag = Arc::new(AtomicBool::new(false));
    {
        let mut ps = state.lock().unwrap();
        ps.stop_flag = Arc::clone(&stop_flag);
    }

    // 3. 启动新播放线程,stop_flag 直接传入(播放线程不再需要回锁 state)。
    let file_path = file_path.to_string();
    let handle = thread::spawn(move || {
        play_sound(&file_path, stop_flag);
    });

    // 4. 记录线程句柄。
    {
        let mut ps = state.lock().unwrap();
        ps.thread_handle = Some(handle);
    }
}

/// 把配置里的键名字符串转成 win-hotkeys 的 VKey 枚举值。
pub fn parse_vkey(name: &str) -> Option<VKey> {
    match name.to_lowercase().as_str() {
        "numpad0" => Some(VKey::Numpad0),
        "numpad1" => Some(VKey::Numpad1),
        "numpad2" => Some(VKey::Numpad2),
        "numpad3" => Some(VKey::Numpad3),
        "numpad4" => Some(VKey::Numpad4),
        "numpad5" => Some(VKey::Numpad5),
        "numpad6" => Some(VKey::Numpad6),
        "numpad7" => Some(VKey::Numpad7),
        "numpad8" => Some(VKey::Numpad8),
        "numpad9" => Some(VKey::Numpad9),
        "f1" => Some(VKey::F1),
        "f2" => Some(VKey::F2),
        "f3" => Some(VKey::F3),
        "f4" => Some(VKey::F4),
        "f5" => Some(VKey::F5),
        "f6" => Some(VKey::F6),
        "f7" => Some(VKey::F7),
        "f8" => Some(VKey::F8),
        "f9" => Some(VKey::F9),
        "f10" => Some(VKey::F10),
        "f11" => Some(VKey::F11),
        "f12" => Some(VKey::F12),
        _ => None,
    }
}

/// 播放线程的主逻辑:切设备 → 播放 → 切回。
/// `stop_flag` 由调用方(handle_hotkey)直接传入,本函数不再回锁共享状态,
/// 从根本上杜绝"回调持锁等 join、线程等锁"的死锁。
fn play_sound(file_path: &str, stop_flag: Arc<AtomicBool>) {
    // 使用状态机的准备逻辑:保存当前麦克风 + 切到 CABLE Output
    let original_id = match state_machine::prepare_playback() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("准备播放失败: {}", e);
            return;
        }
    };

    // DeviceGuard:无论函数怎么退出(包括 panic),Drop 时都会切回原设备
    let _guard = state_machine::DeviceGuard::new(original_id);

    // 播放(可被 stop_flag 中断)
    if let Err(e) = audio_player::play_to_cable_interruptible(file_path, stop_flag) {
        eprintln!("播放失败: {}", e);
    }
}
