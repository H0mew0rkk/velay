//! egui/eframe 图形界面(阶段五主体)。
//!
//! - 主线程跑 UI;控制器线程和热键线程在别处启动,本模块只通过 `Sender<Command>`
//!   和 `SharedState` 与它们交互。
//! - 中文字体在启动时从系统字体目录加载(egui 默认字体无中文,否则显示方块)。
//! - 任意键绑定:进入「录制」态后用 `GetAsyncKeyState` 逐帧扫描,捕捉第一个新按下的键。
//! - 冲突检测:新键若已被其它功能占用则阻止绑定并提示,保留原键。

use crate::app_state::{Command, Shared, SharedState, Status};
use crate::config::{
    Config, DEFAULT_PLAY_PAUSE_VK, DEFAULT_STOP_VK, RoutingMode, Sound, clean_display_name,
    default_sound_name,
};
use crate::hotkeys::{self, HotkeyHandle};
use crate::{app_policy_config, app_state, state_machine};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use win_hotkeys::VKey;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

/// 正在录制哪个热键。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureTarget {
    PlayPause,
    Stop,
    VoiceKey,
    Sound(usize),
}

/// 启动 GUI(阻塞直到窗口关闭)。
pub fn run_gui() -> eframe::Result<()> {
    let config = Config::load();
    let shared: SharedState = Arc::new(Mutex::new(Shared::new(&config)));

    let tx = app_state::spawn_controller(Arc::clone(&shared));
    let _ = tx.send(Command::SetVolume(config.volume));
    let hotkeys = hotkeys::spawn(tx.clone());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([580.0, 680.0])
            .with_min_inner_size([460.0, 480.0])
            .with_title("velay · 语音音效播放器"),
        ..Default::default()
    };

    eframe::run_native(
        "velay",
        native_options,
        Box::new(move |cc| Ok(Box::new(SoundpadApp::new(cc, shared, tx, hotkeys, config)))),
    )
}

struct SoundpadApp {
    config: Config,
    shared: SharedState,
    tx: Sender<Command>,
    hotkeys: HotkeyHandle,

    /// 当前选中行(播放/暂停热键在 Idle 时播它)。
    selected_index: Option<usize>,

    capturing: Option<CaptureTarget>,
    /// 上一帧各 VK 是否按下(边沿检测,长度 256)。
    prev_down: Vec<bool>,
    conflict_msg: Option<String>,

    renaming: Option<usize>,
    rename_buf: String,

    /// 录音设备缓存(GUI 下拉),点「刷新设备」时更新。
    capture_devices: Vec<(String, String)>,
    /// 有可见窗口的程序 `(exe 名, 窗口标题)`——目标程序选择器的默认列表(好认)。
    windowed_apps: Vec<(String, String)>,
    /// 全部运行进程名(兜底:目标程序没有窗口时用)。
    process_names: Vec<String>,
    processes_loaded: bool,
    /// 是否显示完整进程列表(默认只显示有窗口的程序)。
    show_all_processes: bool,
    /// 手动输入目标进程名的输入框。
    target_input: String,
    /// 「紧急清除」等操作的一次性提示。
    notice: Option<String>,
}

impl SoundpadApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        shared: SharedState,
        tx: Sender<Command>,
        hotkeys: HotkeyHandle,
        config: Config,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        let selected_index = if config.sounds.is_empty() { None } else { Some(0) };
        let capture_devices = load_capture_devices();

        // 启动时若还有清不掉的 per-app 残留(目标程序没运行),明确告诉用户,
        // 别让他一头雾水地进游戏发现"说话没人听见"。守望线程已在 main 里挂上了。
        let pending = state_machine::pending_per_app_apps();
        let notice = if pending.is_empty() {
            None
        } else {
            Some(format!(
                "上次未正常退出:{} 的录音设备可能仍被覆盖为虚拟声卡。\
                 它当前没有运行,等它启动后本程序会自动清除;也可现在点下方「紧急恢复」立即清除。",
                pending.join("、")
            ))
        };

        SoundpadApp {
            config,
            shared,
            tx,
            hotkeys,
            selected_index,
            capturing: None,
            prev_down: vec![false; 256],
            conflict_msg: None,
            renaming: None,
            rename_buf: String::new(),
            capture_devices,
            windowed_apps: Vec::new(),
            process_names: Vec::new(),
            processes_loaded: false,
            show_all_processes: false,
            target_input: String::new(),
            notice,
        }
    }

    // ── 音效列表操作 ──

    fn add_sound(&mut self, path: PathBuf) {
        let path_str = path.to_string_lossy().into_owned();
        // 已存在同路径则跳过
        if self.config.sounds.iter().any(|s| s.path == path_str) {
            return;
        }
        let name = default_sound_name(&path_str);
        self.config.sounds.push(Sound {
            name,
            path: path_str,
            hotkey: None,
        });
        if self.selected_index.is_none() {
            self.set_selected(Some(self.config.sounds.len() - 1));
        }
        self.persist();
    }

    fn remove_sound(&mut self, i: usize) {
        if i >= self.config.sounds.len() {
            return;
        }
        self.config.sounds.remove(i);
        // 修正选中索引
        match self.selected_index {
            Some(sel) if sel == i => {
                let new = if self.config.sounds.is_empty() {
                    None
                } else {
                    Some(sel.min(self.config.sounds.len() - 1))
                };
                self.set_selected(new);
            }
            Some(sel) if sel > i => self.set_selected(Some(sel - 1)),
            _ => {}
        }
        if self.renaming == Some(i) {
            self.renaming = None;
        }
        self.persist();
    }

    /// 设置选中行,并同步到共享状态(供播放/暂停热键使用)。
    fn set_selected(&mut self, idx: Option<usize>) {
        self.selected_index = idx;
        if let Ok(mut s) = self.shared.lock() {
            match idx.and_then(|i| self.config.sounds.get(i)) {
                Some(sound) => {
                    s.selected = Some(sound.path.clone());
                    s.selected_name = Some(sound.name.clone());
                }
                None => {
                    s.selected = None;
                    s.selected_name = None;
                }
            }
        }
    }

    fn play_index(&mut self, i: usize) {
        self.set_selected(Some(i));
        if let Some(sound) = self.config.sounds.get(i) {
            let _ = self.tx.send(Command::Play {
                path: sound.path.clone(),
                name: sound.name.clone(),
            });
        }
    }

    /// 重新枚举「有窗口的程序」和「全部进程」,供目标程序选择器使用。
    fn reload_app_lists(&mut self) {
        self.windowed_apps = app_policy_config::list_windowed_apps();
        self.process_names = app_policy_config::list_process_names();
        self.processes_loaded = true;
    }

    /// 添加一个目标进程名(清洗、去重、忽略空值)。
    fn add_target_app(&mut self, name: String) {
        let name = clean_display_name(&name);
        if name.is_empty() {
            return;
        }
        if self
            .config
            .target_apps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&name))
        {
            return;
        }
        self.config.target_apps.push(name);
    }

    // ── 热键录制 ──

    fn begin_capture(&mut self, target: CaptureTarget) {
        self.capturing = Some(target);
        self.conflict_msg = None;
        // 以进入录制那一刻已按下的键为基线,避免「点按钮的回车/已按住的键」被误捕捉。
        self.prev_down = key_snapshot();
    }

    /// 收集除 `skip` 外所有已占用的 VK 码,用于冲突检测。
    fn collect_bound(&self, skip: CaptureTarget) -> Vec<u16> {
        let mut v = Vec::new();
        if skip != CaptureTarget::PlayPause {
            if let Some(k) = self.config.global_hotkeys.play_pause {
                v.push(k);
            }
        }
        if skip != CaptureTarget::Stop {
            if let Some(k) = self.config.global_hotkeys.stop {
                v.push(k);
            }
        }
        if skip != CaptureTarget::VoiceKey {
            if let Some(k) = self.config.auto_mic.voice_key {
                v.push(k);
            }
        }
        for (i, s) in self.config.sounds.iter().enumerate() {
            if skip == CaptureTarget::Sound(i) {
                continue;
            }
            if let Some(k) = s.hotkey {
                v.push(k);
            }
        }
        v
    }

    fn apply_capture(&mut self, vk: u16) {
        let Some(target) = self.capturing else {
            return;
        };
        if self.collect_bound(target).contains(&vk) {
            self.conflict_msg = Some(format!(
                "按键 [{}] 已被其它功能占用,已阻止绑定(保留原键)。",
                key_label(vk)
            ));
            self.capturing = None;
            return;
        }
        match target {
            CaptureTarget::PlayPause => self.config.global_hotkeys.play_pause = Some(vk),
            CaptureTarget::Stop => self.config.global_hotkeys.stop = Some(vk),
            CaptureTarget::VoiceKey => self.config.auto_mic.voice_key = Some(vk),
            CaptureTarget::Sound(i) => {
                if let Some(s) = self.config.sounds.get_mut(i) {
                    s.hotkey = Some(vk);
                }
            }
        }
        self.capturing = None;
        self.conflict_msg = None;
        self.persist_and_reload();
    }

    /// 逐帧扫描按键,捕捉第一个「本帧新按下」的键。Esc 取消。
    fn poll_capture(&mut self, ctx: &egui::Context) {
        if self.capturing.is_none() {
            return;
        }
        ctx.request_repaint(); // 录制期间持续刷新以轮询按键
        let current = key_snapshot();

        // Esc 取消
        if current[0x1B] && !self.prev_down[0x1B] {
            self.capturing = None;
            self.prev_down = current;
            return;
        }
        for vk in 0x07u16..=0xFE {
            let idx = vk as usize;
            if vk == 0x1B {
                continue; // Esc 已处理
            }
            if current[idx] && !self.prev_down[idx] {
                self.apply_capture(vk);
                self.prev_down = current;
                return;
            }
        }
        self.prev_down = current;
    }

    // ── 持久化 ──

    fn persist(&self) {
        self.config.save();
    }

    fn persist_and_reload(&self) {
        self.config.save();
        self.hotkeys.reload();
    }
}

impl eframe::App for SoundpadApp {
    // eframe 0.35:每帧回调改为 `ui`,直接给一个铺满窗口的 Ui;子面板用 show_inside。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // 保持界面与后台状态同步(约每 150ms 刷新一次读取 status)。
        ctx.request_repaint_after(Duration::from_millis(150));

        // 处理拖入的音频文件
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for p in dropped {
            self.add_sound(p);
        }

        // 录制态轮询按键
        self.poll_capture(&ctx);

        // 读取后台状态快照
        let (status, current_name, error) = {
            let s = self.shared.lock().unwrap();
            (s.status, s.current_name.clone(), s.error.clone())
        };

        let before = self.config.clone();

        self.top_panel(ui, status, &current_name);
        self.bottom_panel(ui, status, error);
        self.central_panel(ui);

        // 变更检测:忽略音量(由滑条单独即时保存),其余任意变化即保存并重载热键。
        let mut cmp = before;
        cmp.volume = self.config.volume;
        if cmp != self.config {
            // 若真麦克风 ID 改了,同步友好名
            self.sync_real_mic_name();
            self.persist_and_reload();
        }
    }

    fn on_exit(&mut self) {
        // 停止播放并恢复设备(窗口关闭不触发控制台退出钩子)。
        //
        // 必须**等控制器真正处理完** Shutdown:它要松开语音键、清除 per-app 覆盖、
        // 切回系统默认麦克风。以前只是把命令发出去就往下走,进程可能先退出,
        // 留下"游戏麦克风常开"或"目标程序卡在虚拟声卡"的烂摊子。
        // 超时兜底,别让关窗卡死在一个坏掉的控制器上。
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if self.tx.send(Command::Shutdown(ack_tx)).is_ok() {
            if ack_rx.recv_timeout(Duration::from_secs(3)).is_err() {
                eprintln!("⚠ 控制器未在 3 秒内完成清理,转由退出兜底处理。");
            }
        }
        self.hotkeys.shutdown();

        // 最终兜底:在干净的工作线程(MTA)上同步跑一遍恢复,再让进程退出——
        // 不能在此 STA 主线程上直接跑(COM 助手会因 CoInitializeEx 冲突而放弃恢复)。
        // 控制器已经清干净时这里全是空操作,幂等。
        let _ = std::thread::spawn(state_machine::shutdown_restore).join();
    }
}

impl SoundpadApp {
    fn sync_real_mic_name(&mut self) {
        self.config.real_mic_device_name = match &self.config.real_mic_device_id {
            Some(id) => self
                .capture_devices
                .iter()
                .find(|(d, _)| d == id)
                .map(|(_, n)| n.clone())
                .or_else(|| self.config.real_mic_device_name.clone()),
            None => None,
        };
    }

    fn top_panel(&mut self, ui: &mut egui::Ui, status: Status, current_name: &Option<String>) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("velay");
                ui.separator();
                let (label, color) = match status {
                    Status::Idle => ("● 空闲", egui::Color32::GRAY),
                    Status::Playing => ("● 播放中", egui::Color32::from_rgb(0x3c, 0xb3, 0x71)),
                    Status::Paused => ("● 已暂停", egui::Color32::from_rgb(0xd4, 0xa0, 0x17)),
                };
                ui.colored_label(color, label);
                if let Some(name) = current_name {
                    ui.label(format!("— {name}"));
                }
            });
            ui.add_space(4.0);
        });
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui, status: Status, error: Option<String>) {
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.add_space(6.0);

            if let Some(err) = error {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(0xd9, 0x53, 0x4f), format!("⚠ {err}"));
                    if ui.small_button("关闭").clicked() {
                        if let Ok(mut s) = self.shared.lock() {
                            s.error = None;
                        }
                    }
                });
            }
            if let Some(msg) = self.conflict_msg.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(0xd9, 0x53, 0x4f), format!("⚠ {msg}"));
                    if ui.small_button("知道了").clicked() {
                        self.conflict_msg = None;
                    }
                });
            }
            // 通知(启动时的 per-app 残留提示、紧急恢复的结果):放在这里而不是折叠区里,
            // 免得用户在「全局」模式下根本看不到残留警告。
            if let Some(msg) = self.notice.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(0xd4, 0xa0, 0x17), format!("⚠ {msg}"));
                    if ui.small_button("知道了").clicked() {
                        self.notice = None;
                    }
                });
            }

            ui.horizontal(|ui| {
                let play_label = match status {
                    Status::Playing => "⏸ 暂停",
                    Status::Paused => "▶ 恢复",
                    Status::Idle => "▶ 播放",
                };
                if ui.add(egui::Button::new(play_label).min_size(egui::vec2(88.0, 28.0))).clicked() {
                    let _ = self.tx.send(Command::TogglePlayPause);
                }
                if ui.add(egui::Button::new("■ 停止").min_size(egui::vec2(72.0, 28.0))).clicked() {
                    let _ = self.tx.send(Command::Stop);
                }

                ui.separator();
                ui.label("音量");
                let resp = ui.add(
                    egui::Slider::new(&mut self.config.volume, 0.0..=1.5)
                        .show_value(true)
                        .fixed_decimals(2),
                );
                if resp.changed() {
                    let _ = self.tx.send(Command::SetVolume(self.config.volume));
                }
                if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                    self.persist();
                }
            });
            ui.add_space(6.0);
        });
    }

    fn central_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            // ── 音效列表 ──
            ui.horizontal(|ui| {
                ui.strong("音效列表");
                if ui.button("＋ 添加音频文件").clicked() {
                    if let Some(paths) = rfd::FileDialog::new()
                        .add_filter("音频", &["mp3", "wav", "flac", "ogg"])
                        .pick_files()
                    {
                        for p in paths {
                            self.add_sound(p);
                        }
                    }
                }
                ui.weak("（也可直接把文件拖入窗口）");
            });
            ui.separator();

            let mut to_play: Option<usize> = None;
            let mut to_select: Option<usize> = None;
            let mut to_remove: Option<usize> = None;
            let mut to_capture: Option<usize> = None;
            let mut to_rename_start: Option<usize> = None;
            let mut to_rename_commit: Option<usize> = None;

            egui::ScrollArea::vertical()
                .max_height(260.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.config.sounds.is_empty() {
                        ui.weak("还没有音效,点上方按钮或拖入音频文件添加。");
                    }
                    for i in 0..self.config.sounds.len() {
                        let name = self.config.sounds[i].name.clone();
                        let hotkey = self.config.sounds[i].hotkey;
                        let is_sel = self.selected_index == Some(i);
                        let is_renaming = self.renaming == Some(i);

                        ui.horizontal(|ui| {
                            if is_renaming {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.rename_buf)
                                        .desired_width(180.0),
                                );
                                if ui.button("确定").clicked() {
                                    to_rename_commit = Some(i);
                                }
                                if ui.button("取消").clicked() {
                                    self.renaming = None;
                                }
                            } else {
                                let resp = ui.selectable_label(is_sel, &name);
                                if resp.double_clicked() {
                                    to_play = Some(i);
                                } else if resp.clicked() {
                                    to_select = Some(i);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("删除").clicked() {
                                            to_remove = Some(i);
                                        }
                                        if ui.small_button("改名").clicked() {
                                            to_rename_start = Some(i);
                                        }
                                        let kb = hotkey
                                            .map(key_label)
                                            .unwrap_or_else(|| "—".to_string());
                                        if ui
                                            .small_button(format!("键:{kb}"))
                                            .on_hover_text("为该音效单独绑定热键(高级,可留空)")
                                            .clicked()
                                        {
                                            to_capture = Some(i);
                                        }
                                        if ui.small_button("▶").clicked() {
                                            to_play = Some(i);
                                        }
                                    },
                                );
                            }
                        });
                    }
                });

            // 应用列表操作(循环外,避免借用冲突/索引失效)
            if let Some(i) = to_select {
                self.set_selected(Some(i));
            }
            if let Some(i) = to_play {
                self.play_index(i);
            }
            if let Some(i) = to_capture {
                self.begin_capture(CaptureTarget::Sound(i));
            }
            if let Some(i) = to_rename_start {
                self.renaming = Some(i);
                self.rename_buf = self.config.sounds[i].name.clone();
            }
            if let Some(i) = to_rename_commit {
                let cleaned = clean_display_name(&self.rename_buf);
                if !cleaned.is_empty() {
                    if let Some(s) = self.config.sounds.get_mut(i) {
                        s.name = cleaned;
                    }
                    // 若改的是选中项,同步共享名
                    if self.selected_index == Some(i) {
                        self.set_selected(Some(i));
                    }
                    self.persist();
                }
                self.renaming = None;
            }
            if let Some(i) = to_remove {
                self.remove_sound(i);
            }

            ui.add_space(8.0);
            ui.separator();
            self.settings_ui(ui);
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        // ── 本地监听 ──
        ui.checkbox(
            &mut self.config.local_monitor,
            "本地监听:播放时自己也能同时听到音效(经本机耳机/音箱;下次播放生效)",
        )
        .on_hover_text("音效会同时播到虚拟声卡(队友听到)和你的默认输出设备(你自己听到)。");

        // ── 全局热键(新交互模型) ──
        egui::CollapsingHeader::new("全局热键")
            .default_open(true)
            .show(ui, |ui| {
                self.hotkey_row(ui, "播放 / 暂停", CaptureTarget::PlayPause, self.config.global_hotkeys.play_pause);
                self.hotkey_row(ui, "停止", CaptureTarget::Stop, self.config.global_hotkeys.stop);
                if ui.button("恢复默认热键(F8 / F9)").clicked() {
                    self.config.global_hotkeys.play_pause = Some(DEFAULT_PLAY_PAUSE_VK);
                    self.config.global_hotkeys.stop = Some(DEFAULT_STOP_VK);
                    self.capturing = None;
                    self.conflict_msg = None;
                    self.persist_and_reload();
                }
                ui.weak("在游戏里选中音效后,按「播放/暂停」触发;再按一次暂停(自动切回麦克风)。");
            });

        // ── 播放时自动开麦 ──
        egui::CollapsingHeader::new("播放时自动开麦(高级)")
            .default_open(false)
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.config.auto_mic.enabled,
                    "启用:播放时自动模拟按住游戏语音键",
                );
                self.hotkey_row(ui, "游戏语音键", CaptureTarget::VoiceKey, self.config.auto_mic.voice_key);
                ui.colored_label(
                    egui::Color32::from_rgb(0xd4, 0xa0, 0x17),
                    "注意:这会向游戏发送合成键盘输入(属于「宏」),需在游戏内实测是否有效;\n\
                     若游戏用 Raw Input 过滤合成输入可能无效,此时请改用游戏内「语音激活」模式。",
                );
            });

        // ── 切换范围(阶段七:per-app 路由) ──
        egui::CollapsingHeader::new("切换范围(影响哪些程序)")
            .default_open(true)
            .show(ui, |ui| {
                ui.radio_value(
                    &mut self.config.routing_mode,
                    RoutingMode::System,
                    "全局:切换系统默认麦克风",
                )
                .on_hover_text(
                    "播放期间,所有「跟随系统默认」的程序(不止游戏,也包括微信语音等)都会收到音效。",
                );
                ui.radio_value(
                    &mut self.config.routing_mode,
                    RoutingMode::PerApp,
                    "仅选中程序:只切换下面这些程序的麦克风",
                )
                .on_hover_text("系统默认麦克风不动,微信等其它程序完全不受影响。");

                if self.config.routing_mode == RoutingMode::PerApp {
                    ui.add_space(4.0);
                    ui.separator();

                    // 已选目标程序
                    if self.config.target_apps.is_empty() {
                        ui.weak("还没有选择目标程序。从下面的运行进程里选,或直接输入进程名。");
                    }
                    let mut remove: Option<usize> = None;
                    for (i, app) in self.config.target_apps.clone().iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.monospace(app);
                            if ui.small_button("移除").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove {
                        self.config.target_apps.remove(i);
                    }

                    ui.add_space(4.0);

                    // 首次进入时自动加载——不该逼用户先点「刷新」才看得见可选项。
                    if !self.processes_loaded {
                        self.reload_app_lists();
                    }

                    ui.horizontal(|ui| {
                        ui.strong("选择目标程序");
                        if ui.button("🔄 刷新").on_hover_text("启动游戏后点这里刷新").clicked() {
                            self.reload_app_lists();
                        }
                        ui.checkbox(&mut self.show_all_processes, "显示全部进程")
                            .on_hover_text("默认只列出有窗口的程序(更好认)。若目标程序没有窗口,勾这里看完整进程列表。");
                    });

                    ui.add(
                        egui::TextEdit::singleline(&mut self.target_input)
                            .hint_text("输入关键词筛选,如 cs2 / counter")
                            .desired_width(f32::INFINITY),
                    );

                    // 候选列表:(展示文本, 要写入配置的 exe 名)
                    let filter = self.target_input.trim().to_lowercase();
                    let candidates: Vec<(String, String)> = if self.show_all_processes {
                        self.process_names
                            .iter()
                            .map(|n| (n.clone(), n.clone()))
                            .collect()
                    } else {
                        self.windowed_apps
                            .iter()
                            .map(|(exe, title)| (format!("{title}  —  {exe}"), exe.clone()))
                            .collect()
                    };
                    let matches: Vec<(String, String)> = candidates
                        .into_iter()
                        .filter(|(label, exe)| {
                            filter.is_empty()
                                || label.to_lowercase().contains(&filter)
                                || exe.to_lowercase().contains(&filter)
                        })
                        .collect();

                    if matches.is_empty() {
                        ui.weak("没有匹配的程序。若游戏还没启动,先启动它再点「刷新」。");
                    }

                    let mut pick: Option<String> = None;
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .id_salt("app_pick_list")
                        .show(ui, |ui| {
                            for (label, exe) in &matches {
                                let already = self
                                    .config
                                    .target_apps
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(exe));
                                if ui
                                    .selectable_label(already, label)
                                    .on_hover_text(format!("点击加入:{exe}"))
                                    .clicked()
                                    && !already
                                {
                                    pick = Some(exe.clone());
                                }
                            }
                        });
                    if let Some(exe) = pick {
                        self.add_target_app(exe);
                        self.target_input.clear();
                    }

                    ui.add_space(4.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd4, 0xa0, 0x17),
                        "前提:游戏内语音设备必须是「默认」(CS2 与 Steam 语音都要设为 Default),\n\
                         否则它会锁定具体麦克风,本工具的切换对它无效。",
                    );

                    ui.add_space(4.0);
                    if ui
                        .button("⚠ 紧急恢复:清除所有程序的音频设备覆盖")
                        .on_hover_text(
                            "目标程序卡在虚拟麦克风(游戏里说话没人听见)时的最后手段。\n\
                             通常不需要:程序被强杀后,下次启动会自动清理;若目标程序当时没运行,\n\
                             会等它启动后自动清除。\n\
                             注意:这会清除**所有**应用的 per-app 音频设备设置,包括你在 Windows 设置里手动设过的。",
                        )
                        .clicked()
                    {
                        // 在独立线程里做:GUI 主线程是 STA,而这些 COM 调用按 MTA 初始化。
                        let result = std::thread::spawn(app_policy_config::clear_all_persisted)
                            .join();
                        self.notice = Some(match result {
                            Ok(Ok(())) => {
                                // 覆盖已全部清空,待恢复标记随之失效(守望线程下一轮会自行退出)。
                                state_machine::clear_pending_per_app();
                                "已清除所有应用的音频设备覆盖。".to_string()
                            }
                            Ok(Err(e)) => format!("清除失败:{e}"),
                            Err(_) => "清除线程异常。".to_string(),
                        });
                    }
                    // 结果提示显示在底部通知栏(见 bottom_panel),这里不再重复。
                }
            });

        // ── 设备选择 ──
        egui::CollapsingHeader::new("设备")
            .default_open(false)
            .show(ui, |ui| {
                if ui.button("刷新设备列表").clicked() {
                    self.capture_devices = load_capture_devices();
                }

                // 真麦克风
                ui.horizontal(|ui| {
                    ui.label("真麦克风:");
                    let text = match &self.config.real_mic_device_id {
                        Some(id) => self
                            .capture_devices
                            .iter()
                            .find(|(d, _)| d == id)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| "(已保存设备)".to_string()),
                        None => "(自动:首次播放时记录)".to_string(),
                    };
                    egui::ComboBox::from_id_salt("real_mic")
                        .selected_text(text)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.config.real_mic_device_id,
                                None,
                                "(自动:首次播放时记录)",
                            );
                            for (id, name) in &self.capture_devices {
                                ui.selectable_value(
                                    &mut self.config.real_mic_device_id,
                                    Some(id.clone()),
                                    name,
                                );
                            }
                        });
                });

                // 虚拟声卡录音端(CABLE Output)
                ui.horizontal(|ui| {
                    ui.label("虚拟声卡录音端:");
                    let text = match &self.config.cable_capture_override {
                        Some(id) => self
                            .capture_devices
                            .iter()
                            .find(|(d, _)| d == id)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| "(自定义)".to_string()),
                        None => "(自动识别 CABLE)".to_string(),
                    };
                    egui::ComboBox::from_id_salt("cable_capture")
                        .selected_text(text)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.config.cable_capture_override,
                                None,
                                "(自动识别 CABLE)",
                            );
                            for (id, name) in &self.capture_devices {
                                ui.selectable_value(
                                    &mut self.config.cable_capture_override,
                                    Some(id.clone()),
                                    name,
                                );
                            }
                        });
                });
            });
    }

    /// 一行热键设置:标签 + 当前键 + 录制/取消按钮。
    fn hotkey_row(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        target: CaptureTarget,
        current: Option<u16>,
    ) {
        ui.horizontal(|ui| {
            ui.label(format!("{label}:"));
            let capturing = self.capturing == Some(target);
            if capturing {
                ui.colored_label(
                    egui::Color32::from_rgb(0x3c, 0xb3, 0x71),
                    "按任意键设置…(Esc 取消)",
                );
                if ui.small_button("取消").clicked() {
                    self.capturing = None;
                }
            } else {
                ui.monospace(key_label_opt(current));
                if ui.small_button("录制").clicked() {
                    self.begin_capture(target);
                }
            }
        });
    }
}

// ═══════════════════════════════════════════════════════════════
// 工具函数
// ═══════════════════════════════════════════════════════════════

/// 从系统字体目录加载一个中文字体,插到 egui 字体族最前面。
fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",   // 微软雅黑
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf", // 黑体
        "C:\\Windows\\Fonts\\simsun.ttc", // 宋体
        "C:\\Windows\\Fonts\\Deng.ttf",   // 等线
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            ctx.set_fonts(fonts);
            return;
        }
    }
    eprintln!("⚠ 未能加载系统中文字体,界面中文可能显示为方块。");
}

/// 在独立线程枚举录音设备。GUI 主线程是 STA(winit/OleInitialize),而设备枚举里的
/// `CoInitializeEx(MTA)` 会与之冲突;放到干净的工作线程上跑可拿到 MTA 并正常枚举。
fn load_capture_devices() -> Vec<(String, String)> {
    std::thread::spawn(state_machine::list_capture_devices)
        .join()
        .unwrap_or_default()
}

/// 读取全部 VK 的按下状态快照(索引即 VK 码,长度 256)。
fn key_snapshot() -> Vec<bool> {
    let mut v = vec![false; 256];
    for vk in 0u16..256 {
        v[vk as usize] = is_key_down(vk);
    }
    v
}

fn is_key_down(vk: u16) -> bool {
    // 高位(0x8000)表示当前处于按下状态。
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// VK 码 → 友好键名。win-hotkeys 的 Display 覆盖常见键,未知键回落到十六进制。
fn key_label(vk: u16) -> String {
    let s = VKey::from_vk_code(vk).to_string();
    if s.starts_with("Custom") {
        format!("键(0x{vk:02X})")
    } else {
        s
    }
}

fn key_label_opt(vk: Option<u16>) -> String {
    vk.map(key_label).unwrap_or_else(|| "未绑定".to_string())
}
