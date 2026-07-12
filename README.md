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
- [ ] 阶段二:音频播放模块(rodio 播放 mp3/wav 到指定输出设备)
- [ ] 阶段三:状态机(自动切换的完整逻辑 + 异常兜底)
- [ ] 阶段四:全局热键
- [ ] 阶段五:GUI(egui)
- [ ] 阶段六:打磨与发布

## 环境要求

- Windows 10 / 11
- 已安装 [VB-Cable](https://vb-audio.com/Cable/)(免费,捐赠软件模式)
- Rust stable 工具链

## 使用(当前阶段:命令行设备切换工具)

```
cargo run -- <设备名关键词>
```

按名称关键词(不区分大小写、子串匹配)查找录音设备,并将其设为系统默认录音设备
(Console / Multimedia / Communications 三个角色同时切换)。

```
cargo run -- cable     # 切到 VB-Cable 的 CABLE Output
cargo run -- INZONE    # 切回真实麦克风(示例:INZONE 耳机麦)
```

## 已知风险

- `IPolicyConfig` 是非公开接口,不同 Windows 版本理论上存在 ABI 差异,如遇
  `E_NOINTERFACE` 需要重新核对 GUID / vtable 布局。
- 更多风险与预案见 [soundpad_rust_action_guide.md](soundpad_rust_action_guide.md#9-关键风险总览按优先级)。