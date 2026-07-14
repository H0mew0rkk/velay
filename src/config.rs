//! 配置持久化模块:读写 config.json。
//!
//! 阶段五在原有字段(真麦克风 ID、旧 per-key `hotkeys`)之上,新增了 GUI 需要的
//! 音效列表、全局热键、音量、虚拟声卡覆盖、自动开麦等字段。所有新字段都带
//! `#[serde(default)]`,老配置文件(只有旧字段)仍能正常读入,不会报错。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// 默认「播放/暂停」热键:F8(VK 0x77)。
pub const DEFAULT_PLAY_PAUSE_VK: u16 = 0x77;
/// 默认「停止」热键:F9(VK 0x78)。
pub const DEFAULT_STOP_VK: u16 = 0x78;

/// 单个音效条目。`name` 是展示用名称(默认取文件名去扩展名,已做特殊字符清洗),
/// `path` 是音频文件的绝对路径(原样保存,不做转义)。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Sound {
    pub name: String,
    pub path: String,
    /// 可选:该音效单独绑定的热键(VK 码)。属于「高级」功能,新交互模型下通常为空。
    #[serde(default)]
    pub hotkey: Option<u16>,
}

/// 新交互模型的全局热键(存 Windows 虚拟键码 VK,而非键名,以支持任意键无歧义往返)。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GlobalHotkeys {
    /// 播放 / 暂停(切换)。
    #[serde(default)]
    pub play_pause: Option<u16>,
    /// 停止。
    #[serde(default)]
    pub stop: Option<u16>,
}

impl Default for GlobalHotkeys {
    fn default() -> Self {
        GlobalHotkeys {
            play_pause: Some(DEFAULT_PLAY_PAUSE_VK),
            stop: Some(DEFAULT_STOP_VK),
        }
    }
}

/// 录音设备的切换范围(阶段七)。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// 切换**系统默认**录音设备:所有「跟随默认」的程序(含微信等)播放期间都会收到音效。
    #[default]
    System,
    /// 只切换**选中程序**(如 cs2.exe)的默认录音设备,系统默认不动,其它程序不受影响。
    PerApp,
}

/// 「播放时自动开麦」设置(默认关闭)。开启后播放时用 SendInput 模拟按住游戏语音键。
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoMic {
    #[serde(default)]
    pub enabled: bool,
    /// 游戏内的语音键(VK 码)。为空时即使 enabled 也不会发送按键。
    #[serde(default)]
    pub voice_key: Option<u16>,
}

/// 配置文件。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Config {
    // ── 旧字段(阶段三/四),保持兼容 ──
    /// 真实麦克风的设备 ID
    #[serde(default)]
    pub real_mic_device_id: Option<String>,
    /// 真实麦克风的友好名称
    #[serde(default)]
    pub real_mic_device_name: Option<String>,
    /// 旧 per-key 守护进程:键名(如 "Numpad1")→ 音频路径 / `__STOP__`
    #[serde(default)]
    pub hotkeys: HashMap<String, String>,

    // ── 阶段五新增 ──
    /// 音效列表(GUI 增删)。
    #[serde(default)]
    pub sounds: Vec<Sound>,
    /// 新交互模型的全局热键。
    #[serde(default)]
    pub global_hotkeys: GlobalHotkeys,
    /// 播放音量,0.0 ~ 1.0(可略大于 1.0 放大,GUI 限制到 0~1.5)。
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// 本地监听:播放音效时是否同时播一份到本机默认输出设备(耳机),让自己也能听到。
    #[serde(default = "default_true")]
    pub local_monitor: bool,
    /// 虚拟声卡「录音端」(CABLE Output)的设备 ID 覆盖;为空则按名称自动识别。
    #[serde(default)]
    pub cable_capture_override: Option<String>,
    /// 切换范围:全局(系统默认)还是只针对选中程序。默认全局,保持旧行为。
    #[serde(default)]
    pub routing_mode: RoutingMode,
    /// `routing_mode = per_app` 时的目标进程名列表(如 `["cs2.exe"]`)。
    #[serde(default)]
    pub target_apps: Vec<String>,
    /// 播放时自动开麦设置。
    #[serde(default)]
    pub auto_mic: AutoMic,
}

fn default_volume() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            real_mic_device_id: None,
            real_mic_device_name: None,
            hotkeys: HashMap::new(),
            sounds: Vec::new(),
            global_hotkeys: GlobalHotkeys::default(),
            volume: default_volume(),
            local_monitor: default_true(),
            cable_capture_override: None,
            routing_mode: RoutingMode::default(),
            target_apps: Vec::new(),
            auto_mic: AutoMic::default(),
        }
    }
}

impl Config {
    /// 加载配置,文件不存在或解析失败时返回默认空配置。读入后做一次 sanitize。
    pub fn load() -> Self {
        let path = config_path();
        let mut cfg = if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Config::default()
        };
        cfg.sanitize();
        cfg
    }

    /// 保存配置到 config.json(和 exe 同目录)。
    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// 校正配置,保证热键不冲突、音量在合理范围。用于加载后兜底:
    /// 若全局热键之间(或与自动开麦语音键)互相冲突,冲突项回落到默认键;
    /// 默认键仍冲突时置空(未绑定),不让程序处于矛盾状态。
    pub fn sanitize(&mut self) {
        if !self.volume.is_finite() {
            self.volume = 1.0;
        }
        self.volume = self.volume.clamp(0.0, 1.5);

        // 冲突检测集合:已占用的 VK 码。按优先级依次确认:
        // stop > play_pause > voice_key(优先保留 stop,冲突时改 play_pause/voice_key)。
        let mut used: Vec<u16> = Vec::new();

        // stop:冲突几乎不可能(它先占),保留;为空则给默认。
        let stop = self.global_hotkeys.stop.or(Some(DEFAULT_STOP_VK));
        if let Some(k) = stop {
            used.push(k);
        }
        self.global_hotkeys.stop = stop;

        // play_pause:与已用键冲突则回落默认;默认也冲突则置空。
        self.global_hotkeys.play_pause = resolve_key(
            self.global_hotkeys.play_pause.or(Some(DEFAULT_PLAY_PAUSE_VK)),
            DEFAULT_PLAY_PAUSE_VK,
            &mut used,
        );

        // 自动开麦语音键:与热键冲突会导致「我们发出的语音键」误触发热键,必须避让;
        // 无合适默认,冲突时直接置空并关闭自动开麦。
        if let Some(k) = self.auto_mic.voice_key {
            if used.contains(&k) {
                self.auto_mic.voice_key = None;
                self.auto_mic.enabled = false;
            } else {
                used.push(k);
            }
        }
    }
}

/// 若 `key` 未被占用则采用;否则尝试 `fallback`;`fallback` 也被占用则返回 None。
/// 采用的键会被推入 `used`。
fn resolve_key(key: Option<u16>, fallback: u16, used: &mut Vec<u16>) -> Option<u16> {
    if let Some(k) = key {
        if !used.contains(&k) {
            used.push(k);
            return Some(k);
        }
    }
    if !used.contains(&fallback) {
        used.push(fallback);
        return Some(fallback);
    }
    None
}

/// 从文件路径推导默认音效名:取文件名(去扩展名),并清洗特殊字符——
/// 去掉控制字符、折叠空白、首尾修剪;若清洗后为空则回落到完整文件名,再空则「未命名」。
/// 注意:只影响**显示名**,原始 `path` 原样保存,不受影响。
pub fn default_sound_name(path: &str) -> String {
    let p = std::path::Path::new(path);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let cleaned = clean_display_name(&stem);
    if !cleaned.is_empty() {
        return cleaned;
    }
    let full = clean_display_name(&p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    if full.is_empty() {
        "未命名".to_string()
    } else {
        full
    }
}

/// 清洗展示字符串:剔除控制字符,把连续空白折叠为单个空格,并修剪首尾。
pub fn clean_display_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_control() {
            continue;
        }
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// exe 所在目录。配置和运行时标记文件都放这里。
pub fn exe_dir() -> PathBuf {
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop(); // 去掉 exe 文件名,得到所在目录
    path
}

/// config.json 放在 exe 同目录下。开发期 `cargo run` 时就是项目根目录。
fn config_path() -> PathBuf {
    let mut path = exe_dir();
    path.push("config.json");
    path
}
