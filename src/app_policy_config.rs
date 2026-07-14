//! 手写声明未公开的 `IAudioPolicyConfig` 接口,用于**按进程**设置默认音频设备
//! (per-app 路由,阶段七)。这是 Windows「应用音量和设备首选项 / 音量合成器」
//! 里"给单个程序单独指定输入设备"背后的接口,EarTrumpet 等开源项目用的就是它。
//!
//! 与 [`crate::policy_config`] 的 `IPolicyConfig`(切系统默认)不同:
//! - 它是 WinRT 激活得到的(`RoGetActivationFactory`,类名
//!   `Windows.Media.Internal.AudioPolicyConfig`),基类是 `IInspectable`;
//! - `SetPersistedDefaultAudioEndpoint(processId, flow, role, deviceId)` 只改**该进程**
//!   的默认设备,系统默认不动;`deviceId` 传空表示清除覆盖(该进程恢复跟随系统默认)。
//!
//! GUID / vtable 布局来自社区逆向(以 EarTrumpet 为参考),不同 Windows 版本可能有
//! 差异,失败会返回 `E_NOINTERFACE` 等,由调用方暴露而非静默失败。

#![allow(non_snake_case)]

use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows::core::BOOL;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::Win32::Media::Audio::{
    EDataFlow, ERole, eCapture, eCommunications, eConsole, eMultimedia,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::core::{HRESULT, HSTRING, Interface};

/// WinRT 可激活类名。
const AUDIO_POLICY_CONFIG_CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
/// 录音设备接口 GUID,拼进 SWD 设备路径(SetPersistedDefaultAudioEndpoint 要求这种格式)。
const DEVINTERFACE_AUDIO_CAPTURE: &str = "{2eef81be-33fa-4800-9670-1cd474972c3f}";

/// `IAudioPolicyConfig` 的 vtable。
///
/// 布局(以 EarTrumpet 的 `IAudioPolicyConfigFactoryVariantFor21H2` /
/// `...VariantForDownlevel` 为准——两者**方法顺序完全一致,只有 GUID 不同**):
///
/// ```text
/// 槽位 0..2   IUnknown(QueryInterface / AddRef / Release)
/// 槽位 3..5   IInspectable(GetIids / GetRuntimeClassName / GetTrustLevel)
/// 槽位 6..24  19 个占位方法(CtxVolume / RingerVibrate / VolumeGroup / ChatApplication 等,
///             桌面版多为 E_NOTIMPL,且部分参数是指针——**绝对不能调用**)
/// 槽位 25     SetPersistedDefaultAudioEndpoint  ← 我们要用的
/// 槽位 26     GetPersistedDefaultAudioEndpoint
/// 槽位 27     ClearAllPersistedApplicationDefaultEndpoints(会清空**所有**应用的覆盖,勿调)
/// ```
#[repr(C)]
pub struct IAudioPolicyConfig_Vtbl {
    pub base: windows::core::IUnknown_Vtbl,
    // IInspectable(槽位 3..5)
    pub GetIids: unsafe extern "system" fn() -> HRESULT,
    pub GetRuntimeClassName: unsafe extern "system" fn() -> HRESULT,
    pub GetTrustLevel: unsafe extern "system" fn() -> HRESULT,
    // 19 个占位(槽位 6..24)
    pub ph00: unsafe extern "system" fn() -> HRESULT,
    pub ph01: unsafe extern "system" fn() -> HRESULT,
    pub ph02: unsafe extern "system" fn() -> HRESULT,
    pub ph03: unsafe extern "system" fn() -> HRESULT,
    pub ph04: unsafe extern "system" fn() -> HRESULT,
    pub ph05: unsafe extern "system" fn() -> HRESULT,
    pub ph06: unsafe extern "system" fn() -> HRESULT,
    pub ph07: unsafe extern "system" fn() -> HRESULT,
    pub ph08: unsafe extern "system" fn() -> HRESULT,
    pub ph09: unsafe extern "system" fn() -> HRESULT,
    pub ph10: unsafe extern "system" fn() -> HRESULT,
    pub ph11: unsafe extern "system" fn() -> HRESULT,
    pub ph12: unsafe extern "system" fn() -> HRESULT,
    pub ph13: unsafe extern "system" fn() -> HRESULT,
    pub ph14: unsafe extern "system" fn() -> HRESULT,
    pub ph15: unsafe extern "system" fn() -> HRESULT,
    pub ph16: unsafe extern "system" fn() -> HRESULT,
    pub ph17: unsafe extern "system" fn() -> HRESULT,
    pub ph18: unsafe extern "system" fn() -> HRESULT,
    // 槽位 25 / 26 / 27
    pub SetPersistedDefaultAudioEndpoint:
        unsafe extern "system" fn(*mut c_void, u32, EDataFlow, ERole, *mut c_void) -> HRESULT,
    pub GetPersistedDefaultAudioEndpoint:
        unsafe extern "system" fn(*mut c_void, u32, EDataFlow, ERole, *mut *mut c_void) -> HRESULT,
    pub ClearAllPersistedApplicationDefaultEndpoints:
        unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

// 这个接口在不同 Windows 版本上有两套已知 IID(vtable 布局相同),激活时逐个尝试。
windows::core::imp::define_interface!(
    IAudioPolicyConfigA,
    IAudioPolicyConfig_Vtbl,
    0x2a59116d_6c4f_45e0_a74f_707e3fef9258
);
windows::core::imp::define_interface!(
    IAudioPolicyConfigB,
    IAudioPolicyConfig_Vtbl,
    0xab3d4648_e242_459f_b02f_541c70306324
);

/// 已激活的工厂(不论走哪个 IID,vtable 布局一致,统一用裸指针调用)。
struct Factory {
    raw: *mut c_void,
    vtbl: *const IAudioPolicyConfig_Vtbl,
    /// 记录实际生效的 IID 名称,便于排查。
    variant: &'static str,
    /// 持有原接口对象以维持引用计数,drop 时释放。
    _keep: FactoryKeep,
}

/// 只为持有接口对象、维持 COM 引用计数,字段本身不读取。
#[allow(dead_code)]
enum FactoryKeep {
    A(IAudioPolicyConfigA),
    B(IAudioPolicyConfigB),
}

impl Factory {
    unsafe fn set(&self, pid: u32, role: ERole, device: *mut c_void) -> HRESULT {
        unsafe { ((*self.vtbl).SetPersistedDefaultAudioEndpoint)(self.raw, pid, eCapture, role, device) }
    }

    unsafe fn get(&self, pid: u32, role: ERole) -> windows::core::Result<Option<String>> {
        let mut out: *mut c_void = std::ptr::null_mut();
        unsafe {
            ((*self.vtbl).GetPersistedDefaultAudioEndpoint)(self.raw, pid, eCapture, role, &mut out)
                .ok()?;
        }
        if out.is_null() {
            return Ok(None);
        }
        // out 是一个 HSTRING 句柄,转回 Rust 字符串(所有权归我们,交给 HSTRING 释放)。
        let h: HSTRING = unsafe { std::mem::transmute_copy::<*mut c_void, HSTRING>(&out) };
        let s = h.to_string();
        Ok(Some(s))
    }
}

/// 激活 `IAudioPolicyConfig`:先试 IID-A,失败再试 IID-B。
/// 调用前需当前线程已 `CoInitializeEx`(MTA)。
fn activate() -> windows::core::Result<Factory> {
    let class = HSTRING::from(AUDIO_POLICY_CONFIG_CLASS);

    match unsafe { RoGetActivationFactory::<IAudioPolicyConfigA>(&class) } {
        Ok(f) => {
            let raw = Interface::as_raw(&f);
            let vtbl = Interface::vtable(&f) as *const IAudioPolicyConfig_Vtbl;
            return Ok(Factory {
                raw,
                vtbl,
                variant: "A (2a59116d-…)",
                _keep: FactoryKeep::A(f),
            });
        }
        Err(e_a) => {
            match unsafe { RoGetActivationFactory::<IAudioPolicyConfigB>(&class) } {
                Ok(f) => {
                    let raw = Interface::as_raw(&f);
                    let vtbl = Interface::vtable(&f) as *const IAudioPolicyConfig_Vtbl;
                    Ok(Factory {
                        raw,
                        vtbl,
                        variant: "B (ab3d4648-…)",
                        _keep: FactoryKeep::B(f),
                    })
                }
                Err(e_b) => {
                    eprintln!("⚠ IAudioPolicyConfig 两个 IID 都激活失败:A={e_a} / B={e_b}");
                    Err(e_b)
                }
            }
        }
    }
}

/// 为一组进程设置(或清除)默认**录音**设备,返回**成功的进程数**。
///
/// `device_id` 为原始 MMDevice 端点 ID(如 `{0.0.1.00000000}.{...}`);传 `None` 表示清除
/// 覆盖,让进程恢复跟随系统默认设备。每个进程对三个角色
/// (Console / Multimedia / Communications)各设一次。
///
/// **个别进程失败是正常的,不是错误**:一个应用往往有多个辅助进程(微信实测有 5 个),
/// 音频策略服务只认得真正持有音频会话的那个主进程,对其余进程一律返回 `E_INVALIDARG`。
/// 因此这里逐个进程尝试、统计成功数,由调用方判断「是否至少有一个成功」。
///
/// 整批只激活一次工厂,避免每个进程都走一遍 `RoGetActivationFactory`。
pub fn set_apps_default_capture(pids: &[u32], device_id: Option<&str>) -> usize {
    if pids.is_empty() {
        return 0;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Ok(cfg) = activate() else {
        return 0;
    };

    // 非清除时构造 SWD 设备路径的 HSTRING;整个函数期间保持存活。
    let hstr: Option<HSTRING> = device_id.map(|id| HSTRING::from(swd_path(id)));
    // HSTRING 是指针大小的句柄;作为 [in] 参数只借用,不转移所有权。
    let device_ptr: *mut c_void = match &hstr {
        Some(h) => unsafe { std::mem::transmute_copy::<HSTRING, *mut c_void>(h) },
        None => std::ptr::null_mut(),
    };

    let mut ok = 0usize;
    for &pid in pids {
        // 三个角色分别尝试,不因某个角色失败就放弃整个进程。
        let mut any_role = false;
        for role in [eConsole, eMultimedia, eCommunications] {
            if unsafe { cfg.set(pid, role, device_ptr) }.is_ok() {
                any_role = true;
            }
        }
        if any_role {
            ok += 1;
        }
    }
    ok
}

/// 诊断:对某个进程名的每个 PID、每个角色**分别**尝试设置,逐条打印 HRESULT。
/// 用来定位「哪个角色失败」,而不是笼统地报一个错。
pub fn diag(exe_name: &str, cable_id: &str) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let cfg = match activate() {
        Ok(f) => f,
        Err(e) => {
            println!("激活失败:{e}");
            return;
        }
    };

    let pids = find_process_ids(exe_name);
    if pids.is_empty() {
        println!("没有找到进程:{exe_name}");
        return;
    }
    println!("{exe_name}:{} 个进程", pids.len());

    let h = HSTRING::from(swd_path(cable_id));
    let hptr: *mut c_void = unsafe { std::mem::transmute_copy::<HSTRING, *mut c_void>(&h) };

    for pid in pids {
        println!("\n  pid={pid}");
        for (label, role) in [
            ("Console      ", eConsole),
            ("Multimedia   ", eMultimedia),
            ("Communications", eCommunications),
        ] {
            let hr = unsafe { cfg.set(pid, role, hptr) };
            let tag = if hr.is_ok() { "OK  ✅" } else { "FAIL ❌" };
            println!("    {label} 设为 CABLE : {tag}  0x{:08X}", hr.0 as u32);
        }
        // 清理:把这个进程恢复回去
        for (_, role) in [
            ("", eConsole),
            ("", eMultimedia),
            ("", eCommunications),
        ] {
            let _ = unsafe { cfg.set(pid, role, std::ptr::null_mut()) };
        }
        println!("    (已清除该进程的覆盖)");
    }
}

/// 紧急恢复:清除**所有**应用的 per-app 音频设备覆盖(包括别的软件设过的)。
/// 仅供 GUI 里的「紧急恢复」按钮显式调用,正常流程不要用。
pub fn clear_all_persisted() -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let cfg = activate()?;
    unsafe { ((*cfg.vtbl).ClearAllPersistedApplicationDefaultEndpoints)(cfg.raw).ok() }
}

/// 把 MMDevice 端点 ID 包装成该接口要求的 SWD 设备路径。
fn swd_path(device_id: &str) -> String {
    format!(r"\\?\SWD#MMDEVAPI#{}#{}", device_id, DEVINTERFACE_AUDIO_CAPTURE)
}

/// 按 exe 名(不区分大小写)查找所有匹配进程的 PID。
pub fn find_process_ids(exe_name: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for (pid, name) in enumerate_processes() {
        if name.eq_ignore_ascii_case(exe_name) {
            out.push(pid);
        }
    }
    out
}

/// 列出当前运行进程的去重 exe 名(排序),供 GUI 选择目标程序。
pub fn list_process_names() -> Vec<String> {
    let mut names: Vec<String> = enumerate_processes()
        .into_iter()
        .map(|(_, name)| name)
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by_key(|n| n.to_lowercase());
    names
}

/// 列出「有可见窗口」的程序,返回 `(exe 名, 窗口标题)`,按 exe 名去重排序。
///
/// 比裸进程列表好用得多:用户能直接认出「Counter-Strike 2」,而不必在一百多个
/// 系统进程里辨认 `cs2.exe`,也不用手打进程名(容易打错)。
pub fn list_windowed_apps() -> Vec<(String, String)> {
    let pid_to_name: std::collections::HashMap<u32, String> =
        enumerate_processes().into_iter().collect();

    let mut found: Vec<(u32, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_window_proc),
            LPARAM(&mut found as *mut Vec<(u32, String)> as isize),
        );
    }

    let mut out: Vec<(String, String)> = found
        .into_iter()
        .filter_map(|(pid, title)| pid_to_name.get(&pid).map(|n| (n.clone(), title)))
        .collect();

    // 同一个 exe 可能有多个窗口,只留一个;按 exe 名排序便于查找。
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    out
}

/// `EnumWindows` 回调:收集「可见且有标题」的顶层窗口的 `(pid, 标题)`。
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<(u32, String)>) };

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1); // 继续枚举
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n > 0 {
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                out.push((pid, title));
            }
        }
    }
    BOOL(1)
}

/// 用 Toolhelp 快照枚举 `(pid, exe名)`。
fn enumerate_processes() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                out.push((entry.th32ProcessID, name));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(HANDLE(snap.0));
    }
    out
}

/// 自测:对**本程序自己的进程**把默认录音设为 CABLE Output,读回验证,再清除。
/// 全程不碰任何其它程序,只用来确认 `IAudioPolicyConfig` 在本机能正确绑定并生效。
pub fn selftest(cable_id: &str) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let pid = std::process::id();
    println!("自测 IAudioPolicyConfig(本进程 pid={pid},不影响任何其它程序)\n");

    let cfg = match activate() {
        Ok(f) => {
            println!("① 激活成功,生效变体:{} ✅", f.variant);
            f
        }
        Err(e) => {
            println!("① 激活失败:{e}");
            return;
        }
    };

    println!("② 覆盖前,本进程的默认录音设备:\n     {}", current_capture_label());

    if set_apps_default_capture(&[pid], Some(cable_id)) == 0 {
        println!("③ 设置覆盖失败");
        return;
    }
    println!("③ 设置覆盖 → CABLE:S_OK ✅");

    // 读回验证:证明写进去的确实是我们要的设备路径
    let want = swd_path(cable_id);
    match unsafe { cfg.get(pid, eConsole) } {
        Ok(Some(got)) if got.eq_ignore_ascii_case(&want) => {
            println!("④ 读回一致 ✅\n     {got}")
        }
        Ok(Some(got)) => println!("④ ⚠ 读回不一致:\n     写入 {want}\n     读回 {got}"),
        Ok(None) => println!("④ ⚠ 读回为空(覆盖可能没生效)"),
        Err(e) => println!("④ ⚠ 读回失败:{e}"),
    }

    println!("⑤ 覆盖生效后,本进程的默认录音设备:\n     {}", current_capture_label());

    if set_apps_default_capture(&[pid], None) > 0 {
        println!("⑥ 清除覆盖:S_OK ✅");
    } else {
        println!("⑥ 清除失败");
    }
    println!("⑦ 清除后,本进程的默认录音设备:\n     {}", current_capture_label());
}

/// 当前进程视角下的默认录音设备「名称 [ID]」。
fn current_capture_label() -> String {
    let Some(id) = crate::state_machine::current_default_recording_id() else {
        return "(查询失败)".to_string();
    };
    let name = crate::state_machine::list_capture_devices()
        .into_iter()
        .find(|(d, _)| *d == id)
        .map(|(_, n)| n)
        .unwrap_or_else(|| "(未知设备)".to_string());
    format!("{name}  [{id}]")
}

