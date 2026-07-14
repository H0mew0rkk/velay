//! 「播放时自动开麦」:用 `SendInput` 模拟按住/松开游戏内的语音键。
//!
//! 定位说明:这是向游戏发送合成键盘输入,属于「宏」范畴,已不再是「完全不碰游戏」。
//! 默认关闭;是否有效取决于游戏是否接受合成输入(用 Raw Input 且过滤合成输入的游戏
//! 可能无效,需实测)。**不做驱动级注入。**
//!
//! 安全保证(硬性要求):keyup 用 RAII 守卫兜底——[`AutoMic`] 在 `Drop` 时若仍按着键
//! 会自动松开,避免播放被打断 / 线程 panic / 程序退出时游戏内麦克风一直开着。

use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};

/// 进程级记录「当前正按住的语音键」(0 = 没有按住任何键)。
///
/// 为什么 [`AutoMic`] 的 `Drop` 还不够:按键的「按下」状态是**发给游戏的**,存在游戏
/// 那一侧——我们的进程一旦没来得及发出 keyup 就退出(关窗竞态、被强杀),游戏就会
/// 一直以为你按着语音键,麦克风常开,而且**下次启动本工具也无从知晓**(不像设备覆盖
/// 那样能读回状态)。所以按下时在这里留一份进程级记录,退出路径(GUI on_exit /
/// 控制台退出钩子)统一调 [`release_held_key`] 兜底。
static HELD_VK: AtomicU32 = AtomicU32::new(0);

/// 语音键守卫。持有一个 VK 码,`press` 按下、`release` 松开,内部记录当前是否按住,
/// `Drop` 时确保松开。`press`/`release` 幂等(重复调用不会发出重复事件)。
pub struct AutoMic {
    vk: u16,
    held: bool,
}

impl AutoMic {
    /// 创建一个未按下的语音键守卫。
    pub fn new(vk: u16) -> Self {
        AutoMic { vk, held: false }
    }

    /// 按下语音键(若已按下则无操作)。
    pub fn press(&mut self) {
        if self.held {
            return;
        }
        // 先登记再发键:若在两者之间崩溃,退出钩子会多发一个 keyup(无害);
        // 反过来先发键再登记,崩溃就会漏掉 keyup,游戏麦克风常开。
        HELD_VK.store(self.vk as u32, Ordering::SeqCst);
        send_key(self.vk, false);
        self.held = true;
    }

    /// 松开语音键(若未按下则无操作)。
    pub fn release(&mut self) {
        if !self.held {
            return;
        }
        send_key(self.vk, true);
        HELD_VK.store(0, Ordering::SeqCst);
        self.held = false;
    }
}

impl Drop for AutoMic {
    fn drop(&mut self) {
        // 兜底:异常路径下若还按着,松开它。
        self.release();
    }
}

/// 进程级兜底:若还有语音键按着,松开它。供退出路径(GUI 关窗 / 控制台退出钩子)调用,
/// 不依赖 [`AutoMic`] 实例是否还活着。没有按住任何键时是空操作。
pub fn release_held_key() {
    let vk = HELD_VK.swap(0, Ordering::SeqCst);
    if vk != 0 {
        send_key(vk as u16, true);
        eprintln!("🔙 退出前已松开自动开麦的语音键。");
    }
}

/// 发送一次键盘事件。`key_up == false` 表示按下,`true` 表示松开。
fn send_key(vk: u16, key_up: bool) {
    let flags = if key_up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    unsafe {
        // 一次只发一个事件;失败(返回 0)时无能为力,静默忽略(不影响音频播放主流程)。
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}
