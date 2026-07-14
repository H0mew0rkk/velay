//! 应用共享状态 + 音频控制器线程。
//!
//! 架构:GUI 线程和全局热键线程都只往控制器发 [`Command`](通过 mpsc),控制器线程
//! 独占持有 rodio 的输出流和 `Player`(它们 `!Send`,只能待在一个线程里),并在
//! 播放/暂停/恢复/停止时**跟随切换默认录音设备**:
//!
//! - 播放/恢复 → 默认录音设为 CABLE Output(队友听到音效)
//! - 暂停/停止/播完 → 默认录音切回真麦克风(用户说话队友能听见)
//!
//! 界面显示所需的实时状态(Idle/Playing/Paused、当前音效名、音量、错误)放在
//! [`SharedState`] 里,由控制器更新、GUI 每帧读取。

use crate::audio_player;
use crate::auto_mic::AutoMic;
use crate::config::{Config, RoutingMode};
use crate::state_machine::{self, AppRouteGuard, DeviceGuard};
use rodio::{MixerDeviceSink, Player};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

/// 播放状态机的三态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Playing,
    Paused,
}

/// GUI 与控制器共享的实时状态。
pub struct Shared {
    pub status: Status,
    /// 当前「选中」的音效路径:播放/暂停热键在 Idle 时会播它。由 GUI 更新。
    pub selected: Option<String>,
    pub selected_name: Option<String>,
    /// 正在播放 / 暂停中的音效名(用于界面显示)。
    pub current_name: Option<String>,
    pub volume: f32,
    /// 最近一次错误(找不到 CABLE、文件打不开等),GUI 显示后可清除。
    pub error: Option<String>,
}

impl Shared {
    pub fn new(cfg: &Config) -> Self {
        let first = cfg.sounds.first();
        Shared {
            status: Status::Idle,
            selected: first.map(|s| s.path.clone()),
            selected_name: first.map(|s| s.name.clone()),
            current_name: None,
            volume: cfg.volume,
            error: None,
        }
    }
}

/// 便于在线程间共享。
pub type SharedState = Arc<Mutex<Shared>>;

/// 发给控制器的命令。
pub enum Command {
    /// 播放/暂停切换(新交互模型主路径):Idle→播选中项,Playing→暂停,Paused→恢复。
    TogglePlayPause,
    /// 直接播放指定音效(GUI 双击某行 / per-key 热键)。会打断当前播放。
    Play { path: String, name: String },
    /// 停止。
    Stop,
    /// 设置音量(0.0 ~ 1.5)。
    SetVolume(f32),
    /// 退出控制器线程。附带一个应答通道:控制器**把设备恢复干净之后**才回信,
    /// 调用方(GUI 关窗)据此等待,避免进程先退出导致 per-app 覆盖 / 语音键残留。
    Shutdown(Sender<()>),
}

/// 本次播放采用的录音设备路由方式。播放/恢复时切到 CABLE,暂停/停止时切回真麦——
/// 两种模式只是「切给谁」不同,状态机流程完全一致。
enum Route {
    /// 全局:切换**系统默认**录音设备(旧行为)。所有跟随默认的程序都会受影响。
    System {
        original_mic: String,
        cable_id: String,
        /// RAII:drop 时把系统默认切回真麦。
        _guard: DeviceGuard,
    },
    /// per-app:只切换**目标程序**的默认录音设备,系统默认不动,其它程序不受影响。
    PerApp {
        targets: Vec<String>,
        cable_id: String,
        /// RAII:drop 时清除目标程序的覆盖(恢复跟随系统默认 = 真麦)。
        _guard: AppRouteGuard,
    },
}

impl Route {
    /// 播放 / 恢复:让目标听到 CABLE。
    fn to_cable(&self) {
        match self {
            Route::System { cable_id, .. } => {
                if !cable_id.is_empty() {
                    state_machine::set_default_recording(cable_id);
                }
            }
            Route::PerApp {
                targets, cable_id, ..
            } => {
                state_machine::per_app_to_cable(targets, cable_id);
            }
        }
    }

    /// 暂停 / 停止:把麦克风还给用户(暂停期间说话要能被听见)。
    fn to_real(&self) {
        match self {
            Route::System { original_mic, .. } => state_machine::set_default_recording(original_mic),
            Route::PerApp { targets, .. } => state_machine::per_app_restore(targets),
        }
    }
}

/// 控制器内部持有的一次播放的全部资源。
struct Playback {
    // 输出流句柄:必须与 player 同生命周期,drop 时释放设备。字段仅用于持有,不直接读。
    _handle: MixerDeviceSink,
    player: Player,
    /// 本地监听路(可选):同一音效播到本机默认输出,让用户自己也能听到。
    /// 与主路独立;暂停/恢复/停止/音量需与主路联动。失败时为 None,不影响主路。
    monitor: Option<(MixerDeviceSink, Player)>,
    /// 录音设备路由(含 RAII 守卫,drop 时自动恢复)。
    route: Route,
    /// 自动开麦守卫(启用时);drop 时保证松开语音键。
    auto_mic: Option<AutoMic>,
}

/// 启动控制器线程,返回向它发命令的发送端。
pub fn spawn_controller(shared: SharedState) -> Sender<Command> {
    let (tx, rx) = channel::<Command>();
    thread::spawn(move || controller_loop(rx, shared));
    tx
}

fn controller_loop(rx: Receiver<Command>, shared: SharedState) {
    // 必须在本线程做任何音频/COM 操作**之前**把它初始化成 MTA。
    //
    // 原因:设备切换(IPolicyConfig / IAudioPolicyConfig)要求 MTA,而 cpal 打开音频流时
    // 会尝试把所在线程初始化成 STA。COM 单元「谁先初始化谁赢」——若让 cpal 先占成 STA,
    // 后续切设备的 CoInitializeEx(MTA) 会返回 RPC_E_CHANGED_MODE,表现为「COM 初始化失败」。
    // 反过来我们先占 MTA,cpal 拿到 RPC_E_CHANGED_MODE 会正常容忍(见 cpal wasapi/com.rs)。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut pb: Option<Playback> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(Command::Shutdown(ack)) => {
                // 显式停止:松开语音键、清 per-app 覆盖、切回真麦(都在 stop 的 drop 里)。
                stop(&mut pb, &shared);
                // 全部恢复完成后才回信,调用方可以安全地让进程退出了。
                let _ = ack.send(());
                break;
            }
            Ok(Command::SetVolume(v)) => {
                if let Some(p) = &pb {
                    p.player.set_volume(v);
                    if let Some((_, mp)) = &p.monitor {
                        mp.set_volume(v);
                    }
                }
                if let Ok(mut s) = shared.lock() {
                    s.volume = v;
                }
            }
            Ok(Command::Stop) => stop(&mut pb, &shared),
            Ok(Command::Play { path, name }) => start(&mut pb, &shared, &path, &name),
            Ok(Command::TogglePlayPause) => toggle(&mut pb, &shared),
            Err(RecvTimeoutError::Timeout) => {
                // 检测自然播放结束:Playing 且队列空 → 播完,切回真麦、回到 Idle。
                let finished = pb
                    .as_ref()
                    .map(|p| p.player.empty() && !p.player.is_paused())
                    .unwrap_or(false);
                if finished {
                    finish(&mut pb, &shared);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                stop(&mut pb, &shared);
                break;
            }
        }
    }
}

/// 开始播放指定音效(先停旧的)。
fn start(pb: &mut Option<Playback>, shared: &SharedState, path: &str, name: &str) {
    stop(pb, shared);

    let cfg = Config::load();
    let volume = shared.lock().map(|s| s.volume).unwrap_or(1.0);

    // 1. 先打开播放器。放在最前面:此时还没动任何录音设备,失败可以直接返回,
    //    不会留下「麦克风卡在 CABLE」的烂摊子。
    let (handle, player) = match audio_player::open_player(path, volume) {
        Ok(v) => v,
        Err(e) => {
            set_error(shared, e);
            return;
        }
    };

    // 2. 本地监听(可选):再开一路到默认输出。失败只记日志,不影响主路。
    let monitor = if cfg.local_monitor {
        match audio_player::open_monitor_player(path, volume) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("本地监听未启用: {}", e);
                None
            }
        }
    } else {
        None
    };

    // 3. 建立录音设备路由(含 RAII 守卫)。
    let cable_id = match state_machine::find_cable_capture_id() {
        Ok(id) => id,
        Err(e) => {
            set_error(shared, e);
            return;
        }
    };

    let mut warning: Option<String> = None;
    let route = match cfg.routing_mode {
        RoutingMode::System => {
            // 记录真麦克风并把系统默认切到 CABLE。
            let original = match state_machine::prepare_playback() {
                Ok(id) => id,
                Err(e) => {
                    set_error(shared, e);
                    return;
                }
            };
            Route::System {
                original_mic: original.clone(),
                cable_id,
                _guard: DeviceGuard::new(original),
            }
        }
        RoutingMode::PerApp => {
            if cfg.target_apps.is_empty() {
                set_error(
                    shared,
                    "「仅选中程序」模式下还没有选择目标程序,请先在设置里添加(如 cs2.exe)。"
                        .to_string(),
                );
                return;
            }
            let n = state_machine::per_app_to_cable(&cfg.target_apps, &cable_id);
            if n == 0 {
                // 目标程序没运行:不算错误(本地监听仍然有用),但要提示,
                // 否则用户会疑惑"为什么游戏里没声音"。
                warning = Some(format!(
                    "目标程序({})当前没有在运行,音效只会播到虚拟声卡,没有程序会收到。",
                    cfg.target_apps.join("、")
                ));
            }
            Route::PerApp {
                targets: cfg.target_apps.clone(),
                cable_id,
                _guard: AppRouteGuard::new(cfg.target_apps.clone()),
            }
        }
    };

    // 4. 自动开麦(启用且配置了语音键时):按下语音键。
    let auto_mic = if cfg.auto_mic.enabled {
        cfg.auto_mic.voice_key.map(|vk| {
            let mut a = AutoMic::new(vk);
            a.press();
            a
        })
    } else {
        None
    };

    *pb = Some(Playback {
        _handle: handle,
        player,
        monitor,
        route,
        auto_mic,
    });

    if let Ok(mut s) = shared.lock() {
        s.status = Status::Playing;
        s.current_name = Some(name.to_string());
        s.error = warning;
    }
}

/// 播放/暂停切换。
fn toggle(pb: &mut Option<Playback>, shared: &SharedState) {
    let status = shared.lock().map(|s| s.status).unwrap_or(Status::Idle);
    match status {
        Status::Idle => {
            let sel = shared
                .lock()
                .ok()
                .and_then(|s| Some((s.selected.clone()?, s.selected_name.clone().unwrap_or_default())));
            match sel {
                Some((path, name)) => start(pb, shared, &path, &name),
                None => set_error(shared, "未选择音效,请先在列表里选中一个。".to_string()),
            }
        }
        Status::Playing => pause(pb, shared),
        Status::Paused => resume(pb, shared),
    }
}

/// 暂停:切回真麦克风 + 暂停播放 + 松开语音键。
fn pause(pb: &mut Option<Playback>, shared: &SharedState) {
    if let Some(p) = pb {
        p.player.pause();
        if let Some((_, mp)) = &p.monitor {
            mp.pause();
        }
        // 暂停期间必须把麦克风还回去,否则用户说话没人听见。
        p.route.to_real();
        if let Some(a) = &mut p.auto_mic {
            a.release();
        }
        if let Ok(mut s) = shared.lock() {
            s.status = Status::Paused;
        }
    }
}

/// 恢复:切到 CABLE Output + 继续播放 + 按下语音键。
fn resume(pb: &mut Option<Playback>, shared: &SharedState) {
    if let Some(p) = pb {
        p.route.to_cable();
        p.player.play();
        if let Some((_, mp)) = &p.monitor {
            mp.play();
        }
        if let Some(a) = &mut p.auto_mic {
            a.press();
        }
        if let Ok(mut s) = shared.lock() {
            s.status = Status::Playing;
        }
    }
}

/// 停止:显式停止播放并回到 Idle(guard drop 切回真麦)。
fn stop(pb: &mut Option<Playback>, shared: &SharedState) {
    if let Some(mut p) = pb.take() {
        p.player.stop();
        if let Some((_, mp)) = &p.monitor {
            mp.stop();
        }
        if let Some(a) = &mut p.auto_mic {
            a.release();
        }
        drop(p); // _guard 在此切回真麦克风
        if let Ok(mut s) = shared.lock() {
            s.status = Status::Idle;
            s.current_name = None;
        }
    }
}

/// 自然播完:与停止一致,但不需要主动 `player.stop()`。
fn finish(pb: &mut Option<Playback>, shared: &SharedState) {
    if let Some(mut p) = pb.take() {
        if let Some(a) = &mut p.auto_mic {
            a.release();
        }
        drop(p);
        if let Ok(mut s) = shared.lock() {
            s.status = Status::Idle;
            s.current_name = None;
        }
    }
}

fn set_error(shared: &SharedState, msg: String) {
    eprintln!("控制器错误: {}", msg);
    if let Ok(mut s) = shared.lock() {
        s.status = Status::Idle;
        s.error = Some(msg);
    }
}
