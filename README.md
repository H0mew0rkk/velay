# 目前已可以在命令行中使用

使用方法见 [下方「使用」章节](#使用)。


# soundpad

一个 Windows 下的语音音效播放器(类 [Soundpad](https://enterprise.leppsoft.com/) 的思路),
用 Rust 写。按下热键把指定音频"播"进游戏语音频道(如 CS),平时自动使用真实麦克风,
播放音效时自动切到虚拟麦克风,播放结束自动切回。

详细的技术决策、分阶段计划和风险预案见 [soundpad_rust_action_guide.md](soundpad_rust_action_guide.md)。

## 原理

- 依赖 [VB-Cable](https://vb-audio.com/Cable/) 提供虚拟麦克风设备,不自研音频驱动。
- 不做"混音同播",而是在需要播放音效时把系统默认录音设备切换成虚拟麦克风,
  播放结束后再切回真实麦克风。
- 设备切换通过 Windows 非公开 COM 接口 `IPolicyConfig::SetDefaultEndpoint` 实现——
  这是业内切换默认音频设备的通行做法(EarTrumpet、AudioSwitcher 等工具同款思路),
  不注入游戏进程、不读写游戏内存。

## 当前进度

- [x] 阶段〇:手动验证"放音乐 → 游戏内队友能听到 → 切回麦克风说话正常"整条链路可行
- [x] 阶段一:设备切换模块——命令行工具,可枚举录音设备并切换系统默认录音设备
- [x] 阶段二:音频播放模块(rodio 播放 mp3/wav 到指定输出设备)
- [x] 阶段三:状态机(自动切换的完整逻辑 + 异常兜底:启动自检、配置持久化、退出钩子)
- [x] 阶段四:全局热键(win-hotkeys,底层 WH_KEYBOARD_LL 低级键盘钩子;已实测可用)
- [ ] 阶段五:GUI(egui)
- [ ] 阶段六:打磨与发布

## 环境要求

- Windows 10 / 11
- 已安装 [VB-Cable](https://vb-audio.com/Cable/)(免费,捐赠软件模式)
- Rust stable 工具链

## 使用

### 运行方式

开发期(没有单独编译)在项目根目录用 `cargo run --` 加子命令,`--` 后面的内容
会原样传给程序:

```
cargo run -- daemon
cargo run -- play C:\sounds\hello.mp3
```

发布后(`cargo build --release` 产出的 `soundpad.exe`)直接把子命令跟在 exe
后面即可,两种方式命令行为完全一致:

```
soundpad.exe daemon
soundpad.exe play C:\sounds\hello.mp3
```

下文统一用 `soundpad <子命令>` 指代,开发期请自行替换成 `cargo run -- <子命令>`。

### 命令列表

```
soundpad daemon         启动热键守护进程(主要用法,游戏内按热键触发音效)
soundpad play <文件>    播放单个音频文件(自动切换录音设备,阻塞到播完/按 Enter)
soundpad devices        列出所有输出设备(找 CABLE Input 用)
soundpad rec            列出所有录音设备(找真麦克风/CABLE Output 用)
soundpad <关键词>       手动切换默认录音设备(不区分大小写、子串匹配,调试用)
```

### 调整设置(config.json)

所有设置都存在 exe 同目录的 `config.json` 里,没有独立的设置命令,直接编辑
这个文件后重新运行 `soundpad daemon` 生效:

| 字段 | 说明 |
|---|---|
| `hotkeys` | 热键 → 音频文件的映射,见下方"热键守护进程"一节 |
| `real_mic_device_id` | 真实麦克风的设备 ID,首次播放会自动写入;需要改回其他麦克风时,手动运行 `soundpad <新麦克风关键词>` 切换后再触发一次播放即可自动更新 |
| `real_mic_device_name` | 对应上一字段的友好名称,仅用于日志显示,无需手动改 |

文件不存在或字段缺失时程序会用默认值(空),不会报错;`hotkeys` 为空时
`soundpad daemon` 会打印配置示例并退出,提示先编辑。

### 热键守护进程(daemon)

首次运行会提示 `config.json` 中缺少 `hotkeys` 配置。在 exe 同目录的 `config.json`
里添加热键 → 音频文件的映射,键名支持 `NumpadN`(小键盘 0-9)和 `F1`-`F12`,
其中一个键的值固定写 `__STOP__` 作为停止键:

```json
{
  "hotkeys": {
    "Numpad1": "C:\\sounds\\hello.mp3",
    "Numpad2": "C:\\sounds\\laugh.mp3",
    "Numpad0": "__STOP__"
  }
}
```

保存后重新运行 `soundpad daemon`。真实麦克风的设备 ID 会在首次播放时自动记录进
`config.json` 的 `real_mic_device_id` 字段,之后每次播放都会自动切回。

### 退出与异常恢复

- 正常关闭 daemon 窗口 / 按 Ctrl+C:退出前会自动把录音设备切回真麦克风。
- 若被任务管理器强制结束进程,退出钩子不会触发,麦克风可能卡在 CABLE Output——
  下次运行 `soundpad`(任意命令)时会自动检测并切回,也可手动运行
  `soundpad <麦克风关键词>` 立即切回。

## 已知风险

- `IPolicyConfig` 是非公开接口,不同 Windows 版本理论上存在 ABI 差异,如遇
  `E_NOINTERFACE` 需要重新核对 GUID / vtable 布局(现已在切换失败时打印错误,
  不会再静默失败)。
- 强制结束进程(任务管理器杀进程等)不会触发退出钩子,麦克风可能暂时卡在
  CABLE Output,需下次启动或手动切回,见上文"退出与异常恢复"。
- 更多风险与预案见 [soundpad_rust_action_guide.md](soundpad_rust_action_guide.md#9-关键风险总览按优先级)。