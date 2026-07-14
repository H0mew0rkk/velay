//! per-app 覆盖的「待恢复」标记(阶段七兜底的补丁,见行动指南 14.5 ①)。
//!
//! 为什么需要它:`SetPersistedDefaultAudioEndpoint` 只能通过**活着的 PID** 定位应用,
//! 目标程序没运行时根本清不掉它的覆盖;而覆盖是持久化的(重启不消失)。于是存在
//! 一个盲区:per-app 播放中被强杀 → 游戏也关了 → 下次启动本工具时目标程序不在 →
//! 启动自检拿不到 PID → 残留清不掉 → 之后开游戏,它的麦克风一直是虚拟声卡。
//!
//! 对策:每次给目标程序设覆盖前先把「设给了谁」落盘;恢复成功后删除标记。启动时若
//! 标记还在,说明上次没恢复干净——目标程序在就立刻清,不在就交给守望线程等它出现。
//!
//! 为什么不放进 config.json:GUI 持有一份 `Config` 副本并会**整体写回**,控制器线程
//! 写进 config 的标记会被 GUI 的下一次保存悄悄覆盖掉。独立小文件不参与那场竞争。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
struct Pending {
    /// 上次设过覆盖、尚未确认恢复的目标进程名。
    apps: Vec<String>,
}

fn path() -> PathBuf {
    let mut p = crate::config::exe_dir();
    p.push("per_app_pending.json");
    p
}

/// 记录「已给这些程序设了覆盖」。设覆盖**之前**调用:即使在写标记和设覆盖之间崩溃,
/// 也只是下次多清一遍(清除是幂等的),总好过漏清。
pub fn mark(apps: &[String]) {
    let data = Pending {
        apps: apps.to_vec(),
    };
    if let Ok(json) = serde_json::to_string(&data) {
        let _ = std::fs::write(path(), json);
    }
}

/// 覆盖已确认清除,删除标记。
pub fn clear() {
    let _ = std::fs::remove_file(path());
}

/// 读取尚未确认恢复的目标程序;没有标记时返回空。
pub fn pending_apps() -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Pending>(&content)
        .map(|p| p.apps)
        .unwrap_or_default()
}
