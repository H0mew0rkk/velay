//! 全局热键(GUI 模式)——自研的常驻低级键盘钩子。
//!
//! 为什么不用 win-hotkeys 的 `event_loop`:它的钩子基于**进程级全局静态通道**,
//! 且要求持续独占一套通道;想在运行时改键只能「中断 event_loop → 再次 event_loop」,
//! 而后者会重装第二个 WH_KEYBOARD_LL 钩子、覆盖全局通道,导致钩子相互阻塞、
//! 整个系统键盘卡死。
//!
//! 这里改为:**装一次钩子,常驻**;维护一张共享的「VK 码 → 动作」表,改键时只替换
//! 这张表(加锁换表),钩子本身不动。钩子回调命中则发一条 [`Command`] 给控制器,
//! 并吞掉该按键(与阶段四 win-hotkeys 的拦截行为一致)。
//!
//! CLI 的 `daemon` 子命令仍用 win-hotkeys(它只在主线程跑一次 event_loop、从不重建,
//! 不受上述问题影响)。

use crate::app_state::Command;
use crate::config::{Config, default_sound_name};
use crate::hotkey_daemon;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::thread;
use win_hotkeys::VKey;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// 一个热键对应的动作。
#[derive(Clone)]
enum Action {
    TogglePlayPause,
    Stop,
    Play { path: String, name: String },
}

/// 钩子运行时:VK→动作 表 + 发命令的通道。由钩子回调(在钩子线程)读取,
/// 由 GUI 线程改键时替换 `map`。全程加锁,回调内只做「查表 + 发送」的极快操作。
struct Runtime {
    map: HashMap<u16, Action>,
    tx: Sender<Command>,
}

static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();
/// 钩子线程 ID,退出时用 `PostThreadMessageW(WM_QUIT)` 唤醒其消息循环以卸载钩子。
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// 当前物理上处于「按下」状态的热键集合,用于去抖:按住不放时系统会重复投递
/// KeyDown,这里保证同一个键只在「抬起→按下」的那一次触发,重复的按下被忽略。
static PRESSED: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();

fn runtime() -> &'static Mutex<Option<Runtime>> {
    RUNTIME.get_or_init(|| Mutex::new(None))
}

fn pressed() -> &'static Mutex<HashSet<u16>> {
    PRESSED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 打印当前已绑定的热键(诊断用:确认钩子装上了、且表里真的有键)。
fn print_bound_keys() {
    let Ok(guard) = runtime().lock() else {
        eprintln!("⚠ 热键运行时被占用,无法打印绑定。");
        return;
    };
    let Some(rt) = guard.as_ref() else {
        eprintln!("⚠ 热键运行时为空,没有任何绑定!");
        return;
    };
    if rt.map.is_empty() {
        eprintln!("⚠ 键盘钩子已安装,但**没有绑定任何热键**。");
        return;
    }
    eprintln!("✅ 键盘钩子已安装,已绑定 {} 个热键:", rt.map.len());
    let mut keys: Vec<_> = rt.map.iter().collect();
    keys.sort_by_key(|(vk, _)| **vk);
    for (vk, action) in keys {
        let what = match action {
            Action::TogglePlayPause => "播放/暂停".to_string(),
            Action::Stop => "停止".to_string(),
            Action::Play { name, .. } => format!("播放「{name}」"),
        };
        eprintln!("   {} (VK 0x{:02X}) → {}", key_name(*vk), vk, what);
    }
}

/// VK 码 → 友好键名。
fn key_name(vk: u16) -> String {
    let s = VKey::from_vk_code(vk).to_string();
    if s.starts_with("Custom") {
        format!("键(0x{vk:02X})")
    } else {
        s
    }
}

/// 由配置构建 VK→动作 表。用 `entry().or_insert` 保证「先到先得」:
/// 播放/暂停、停止优先于 per-key 与旧配置,重复键不会互相覆盖。
fn build_map(cfg: &Config) -> HashMap<u16, Action> {
    let mut map: HashMap<u16, Action> = HashMap::new();

    if let Some(vk) = cfg.global_hotkeys.play_pause {
        map.entry(vk).or_insert(Action::TogglePlayPause);
    }
    if let Some(vk) = cfg.global_hotkeys.stop {
        map.entry(vk).or_insert(Action::Stop);
    }
    for s in &cfg.sounds {
        if let Some(vk) = s.hotkey {
            map.entry(vk).or_insert_with(|| Action::Play {
                path: s.path.clone(),
                name: s.name.clone(),
            });
        }
    }
    // 兼容旧 per-key 配置(hotkeys 映射表:键名 → 路径 / __STOP__)
    for (key_name, target) in &cfg.hotkeys {
        let Some(vkey) = hotkey_daemon::parse_vkey(key_name) else {
            continue;
        };
        let vk = vkey.to_vk_code();
        if target == "__STOP__" {
            map.entry(vk).or_insert(Action::Stop);
        } else {
            let path = target.clone();
            let name = default_sound_name(target);
            map.entry(vk).or_insert(Action::Play { path, name });
        }
    }
    map
}

/// 控制热键的句柄:改键调 [`reload`](Self::reload),退出调 [`shutdown`](Self::shutdown)。
pub struct HotkeyHandle {
    _private: (),
}

impl HotkeyHandle {
    /// 按最新 config.json 重建 VK→动作 表(立即生效,不重装钩子)。
    pub fn reload(&self) {
        let map = build_map(&Config::load());
        if let Ok(mut guard) = runtime().lock() {
            if let Some(rt) = guard.as_mut() {
                rt.map = map;
            }
        }
    }

    /// 退出:清空动作表(立即停止响应)并请求钩子线程卸载钩子退出。
    pub fn shutdown(&self) {
        if let Ok(mut guard) = runtime().lock() {
            *guard = None;
        }
        let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// 启动常驻热键钩子。回调把命令发到 `tx`。
pub fn spawn(tx: Sender<Command>) -> HotkeyHandle {
    // 初始化运行时(表 + 通道)
    {
        let map = build_map(&Config::load());
        let mut guard = runtime().lock().unwrap();
        *guard = Some(Runtime { map, tx });
    }

    thread::spawn(|| unsafe {
        HOOK_THREAD_ID.store(windows::Win32::System::Threading::GetCurrentThreadId(), Ordering::SeqCst);

        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("⚠ 无法安装键盘钩子,全局热键不可用: {e}");
                return;
            }
        };
        print_bound_keys();

        // 消息循环:低级键盘钩子要求安装线程持续泵消息;GetMessageW 收到 WM_QUIT 返回 false。
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
    });

    HotkeyHandle { _private: () }
}

/// WH_KEYBOARD_LL 回调。命中动作表则发命令并吞键(返回非 0);否则放行。
/// 必须极快返回,内部只做「加锁查表 + 发送 mpsc」。
///
/// 去抖:按住不放的键会重复投递 KeyDown;这里用 [`PRESSED`] 记录当前按下的热键,
/// 只在「抬起→按下」的首次触发,重复按下与抬起都吞掉但不重复发命令。
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let injected = (kb.flags.0 & LLKHF_INJECTED.0) != 0;
        let vk = kb.vkCode as u16;
        let event = wparam.0 as u32;
        let is_down = event == WM_KEYDOWN || event == WM_SYSKEYDOWN;
        let is_up = event == WM_KEYUP || event == WM_SYSKEYUP;

        // 忽略合成输入(如自动开麦自己发的键),避免自触发。
        if !injected && (is_down || is_up) {
            // 抬起:无条件清除按下标记(即使该键已被解绑),避免残留导致下次不触发。
            if is_up {
                if let Ok(mut set) = pressed().lock() {
                    set.remove(&vk);
                }
            }

            // 查该键是否为已绑定热键,顺带取出动作与发送端(在锁内 clone,尽快释放锁)。
            let hit: Option<(Action, Sender<Command>)> = match RUNTIME.get() {
                Some(m) => match m.lock() {
                    Ok(g) => g
                        .as_ref()
                        .and_then(|rt| rt.map.get(&vk).cloned().map(|a| (a, rt.tx.clone()))),
                    Err(_) => None,
                },
                None => None,
            };

            if let Some((action, tx)) = hit {
                if is_down {
                    // 去抖:仅当该键之前不在按下集合里(首次按下)才发命令。
                    let is_first = pressed().lock().map(|mut s| s.insert(vk)).unwrap_or(true);
                    if is_first {
                        let cmd = match action {
                            Action::TogglePlayPause => Command::TogglePlayPause,
                            Action::Stop => Command::Stop,
                            Action::Play { path, name } => Command::Play { path, name },
                        };
                        if tx.send(cmd).is_err() {
                            eprintln!("⚠ 热键命令发送失败(控制器已退出?)");
                        }
                    }
                }
                // 吞掉热键的按下(含重复)与抬起,不让游戏/其它程序收到。
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
