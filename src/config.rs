//! 配置持久化模块:读写 config.json。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// 配置文件。热键映射 key 是 VKey 变体名(如 "Numpad1"),
/// value 是音频文件的绝对路径。
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Config {
    /// 真实麦克风的设备 ID
    #[serde(default)]
    pub real_mic_device_id: Option<String>,
    /// 真实麦克风的友好名称
    #[serde(default)]
    pub real_mic_device_name: Option<String>,
    /// 热键 → 音频文件路径 的映射表
    /// 示例: { "Numpad1": "C:\\sounds\\hello.mp3", "Numpad0": "__STOP__" }
    #[serde(default)]
    pub hotkeys: HashMap<String, String>,
}

impl Config {
    /// 加载配置,文件不存在时返回默认空配置。
    pub fn load() -> Self {
        let path = config_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Config::default()
        }
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
}

/// config.json 放在 exe 同目录下。
/// 开发期 `cargo run` 时就是项目根目录。
fn config_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop(); // 去掉 exe 文件名,得到所在目录
    path.push("config.json");
    path
}
