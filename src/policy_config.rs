//! 手写声明微软未公开文档化的 `IPolicyConfig` COM 接口。
//! GUID 与方法顺序来自社区逆向资料(Windows Vista ~ 11 通用做法),
//! 官方 windows-rs crate 不包含这个接口,因此需要自己声明 vtable。
//! 若某个 Windows 版本的 ABI 有差异,调用会返回 E_NOINTERFACE。

#![allow(non_snake_case)]

use std::ffi::c_void;
use windows::Win32::Media::Audio::ERole;
use windows::core::{GUID, HRESULT, Interface, PCWSTR};

/// CLSID_PolicyConfigClient:系统里实现 IPolicyConfig 的那个 COM 类的类 ID。
pub const CLSID_POLICY_CONFIG_CLIENT: GUID =
    GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// IPolicyConfig 的虚函数表(vtable)。
/// 前 3 个方法(QueryInterface/AddRef/Release)来自 IUnknown,所有 COM 接口都有。
/// 之后按官方声明顺序原样列出;我们只会调用 `SetDefaultEndpoint`,
/// 其余方法的参数类型用 `*mut c_void` 占位即可——
/// 因为 vtable 本质是一串函数指针,大小都一样,占位不影响内存布局,
/// 只要"数量和顺序"对得上,能找到 SetDefaultEndpoint 在第几位就行。
#[repr(C)]
pub struct IPolicyConfig_Vtbl {
    pub base: windows::core::IUnknown_Vtbl,
    pub GetMixFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut *mut c_void) -> HRESULT,
    pub GetDeviceFormat:
        unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut *mut c_void) -> HRESULT,
    pub ResetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    pub SetDeviceFormat:
        unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void, *mut c_void) -> HRESULT,
    pub GetProcessingPeriod:
        unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut i64, *mut i64) -> HRESULT,
    pub SetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut i64) -> HRESULT,
    pub GetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
    pub SetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
    // ⚠ 警告:下面 Get/SetPropertyValue 的参数列表**并不完整/准确**——
    // 真实接口在不同逆向资料里对是否含 `BOOL bFxStore` 参数存在分歧。
    // 这里只作为 vtable 占位以保证 SetDefaultEndpoint 的槽位序号正确,
    // **切勿直接调用这两个方法**,否则栈参数会错乱。
    pub GetPropertyValue:
        unsafe extern "system" fn(*mut c_void, PCWSTR, *const c_void, *mut c_void) -> HRESULT,
    pub SetPropertyValue:
        unsafe extern "system" fn(*mut c_void, PCWSTR, *const c_void, *const c_void) -> HRESULT,
    pub SetDefaultEndpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
    pub SetEndpointVisibility: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
}

// 用官方宏把上面的 vtable 结构体和一个 GUID(接口 ID)绑定成一个可用的 COM 接口类型。
// 这一行生成了 `IPolicyConfig` 这个结构体本身,以及让它能被 CoCreateInstance 识别的实现。
windows::core::imp::define_interface!(
    IPolicyConfig,
    IPolicyConfig_Vtbl,
    0xf8679f50_850a_41cf_9c72_430f290290c8
);

impl IPolicyConfig {
    /// 把 `device_id` 对应的录音/播放设备设为默认设备。
    /// `role` 对应 Windows 内部的三种"用途角色":Console(系统提示音等)/
    /// Multimedia(媒体播放)/Communications(通话软件),
    /// 三者可以各自有不同的默认设备,所以完整切换要三个角色都调一遍。
    pub unsafe fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> windows::core::Result<()> {
        unsafe {
            (Interface::vtable(self).SetDefaultEndpoint)(Interface::as_raw(self), device_id, role)
                .ok()
        }
    }
}