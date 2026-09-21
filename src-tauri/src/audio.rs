//! 真实音频能力（Windows Core Audio）：
//! - 枚举输出/输入设备
//! - 切换默认设备（IPolicyConfig::SetDefaultEndpoint，三个角色同时设置）
//! - 读取/设置端点音量与静音（IAudioEndpointVolume）
// COM 接口方法名必须保留原始大小写（与 vtable 声明一致），屏蔽 snake_case 提示
#![allow(non_snake_case)]
use serde::Serialize;
use std::path::Path;
use windows::core::{interface, Interface, PCWSTR, GUID, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2,
    ISimpleAudioVolume, AudioSessionStateActive, AudioSessionStateExpired, IMMDevice,
    IMMDeviceEnumerator, EDataFlow, ERole, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX, CLSCTX_ALL, CLSCTX_LOCAL_SERVER,
    COINIT_MULTITHREADED, STGM,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

#[derive(Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Serialize)]
pub struct EndpointVolume {
    pub volume: u32, // 0-100
    pub muted: bool,
}

/// PolicyConfigClient（未公开 COM 接口，SoundSwitch 等同类工具的标准做法）。
/// 不同 Windows 版本暴露的接口 IID 不同，SetDefaultEndpoint 槽位也不同：
/// - Win10/11 主力：f8679f50-850a-41cf-9c72-430f290290c8（SetDefaultEndpoint 在槽位 13）
/// - 较新构建：294935ce-f637-4e7c-a41b-ab255460b862（同 Win10 布局）
/// - Vista 版：ca286fc3-91fd-42c3-8e9b-caafa66242e3（同布局）
/// - Win7 老版：568b9108-44bf-40b4-9006-86afe5b5a620（SetDefaultEndpoint 在槽位 9）
// 完整布局：IUnknown(3) + 10 个占位 + SetDefaultEndpoint = 槽位 13
macro_rules! policy_full {
    ($name:ident, $iid:literal) => {
        #[interface($iid)]
        unsafe trait $name: ::windows_core::IUnknown {
            fn GetMixFormat(&self, a: *const core::ffi::c_void, b: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
            fn GetDeviceFormat(&self, a: *const core::ffi::c_void, b: bool, c: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
            fn ResetDeviceFormat(&self, a: *const core::ffi::c_void) -> windows::core::Result<()>;
            fn SetDeviceFormat(&self, a: *const core::ffi::c_void, b: *mut core::ffi::c_void, c: *mut core::ffi::c_void) -> windows::core::Result<()>;
            fn GetProcessingPeriod(&self, a: *const core::ffi::c_void, b: bool, c: *mut i64, d: *mut i64) -> windows::core::Result<()>;
            fn SetProcessingPeriod(&self, a: *const core::ffi::c_void, b: *const i64) -> windows::core::Result<()>;
            fn GetShareMode(&self, a: *const core::ffi::c_void, b: *mut i32) -> windows::core::Result<()>;
            fn SetShareMode(&self, a: *const core::ffi::c_void, b: i32) -> windows::core::Result<()>;
            fn GetPropertyValue(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *mut core::ffi::c_void) -> windows::core::Result<()>;
            fn SetPropertyValue(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *const core::ffi::c_void) -> windows::core::Result<()>;
            fn SetDefaultEndpoint(&self, pszwdeviceid: PCWSTR, erole: ERole) -> windows::core::Result<()>;
            fn SetEndpointVisibility(&self, a: *const core::ffi::c_void, b: bool) -> windows::core::Result<()>;
        }
    };
}

policy_full!(PolicyConfigWin10, "f8679f50-850a-41cf-9c72-430f290290c8");
policy_full!(PolicyConfigNew, "294935ce-f637-4e7c-a41b-ab255460b862");
policy_full!(PolicyConfigVista, "ca286fc3-91fd-42c3-8e9b-caafa66242e3");

#[interface("568b9108-44bf-40b4-9006-86afe5b5a620")]
unsafe trait PolicyConfigLegacy: ::windows_core::IUnknown {
    fn GetMixFormat(&self, a: *const core::ffi::c_void, b: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn GetDeviceFormat(&self, a: *const core::ffi::c_void, b: bool, c: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn SetDeviceFormat(&self, a: *const core::ffi::c_void, b: *mut core::ffi::c_void, c: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn GetPropertyValue(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn SetPropertyValue(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn SetDefaultEndpoint(&self, pszwdeviceid: PCWSTR, erole: ERole) -> windows::core::Result<()>;
    fn SetEndpointVisibility(&self, a: *const core::ffi::c_void, b: bool) -> windows::core::Result<()>;
}

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
#[allow(unused_imports)]
use windows::core::IUnknown as _;

pub const CLSID_TEST: GUID = CLSID_POLICY_CONFIG_CLIENT;

fn com_init() {
    // 线程池线程可能已被初始化为 STA，忽略该错误即可
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

fn flow_of(kind: &str) -> Result<EDataFlow, String> {
    match kind {
        "output" => Ok(EDataFlow(0)), // eRender
        "input" => Ok(EDataFlow(1)),  // eCapture
        _ => Err("kind 只能为 output 或 input".into()),
    }
}

fn device_name(dev: &IMMDevice) -> String {
    let store = unsafe { dev.OpenPropertyStore(STGM(0)) }; // STGM_READ
    let store = match store {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let pv = match unsafe { store.GetValue(&PKEY_Device_FriendlyName) } {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let mut name = String::new();
    unsafe {
        if let Ok(pw) = PropVariantToStringAlloc(&pv) {
            name = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as _));
        }
    }
    name
}

fn device_id(dev: &IMMDevice) -> String {
    match unsafe { dev.GetId() } {
        Ok(id) => {
            let s = unsafe { id.to_string().unwrap_or_default() };
            unsafe { CoTaskMemFree(Some(id.0 as _)) };
            s
        }
        Err(_) => String::new(),
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    com_init();
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| e.to_string())
}

pub fn list_devices(kind: &str) -> Result<Vec<AudioDevice>, String> {
    let flow = flow_of(kind)?;
    let enumerator = enumerator()?;
    unsafe {
        let default_id = enumerator
            .GetDefaultAudioEndpoint(flow, ERole(0))
            .ok()
            .map(|d| device_id(&d))
            .unwrap_or_default();
        let coll = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(|e| e.to_string())?;
        let count = coll.GetCount().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for i in 0..count {
            let dev = coll.Item(i).map_err(|e| e.to_string())?;
            let id = device_id(&dev);
            if id.is_empty() {
                continue;
            }
            out.push(AudioDevice {
                name: device_name(&dev),
                is_default: id == default_id,
                id,
            });
        }
        Ok(out)
    }
}

#[tauri::command]
pub async fn list_audio_devices(kind: String) -> Result<Vec<AudioDevice>, String> {
    list_devices(&kind)
}

pub fn switch_default(device_id: &str) -> Result<(), String> {
    if device_id.is_empty() {
        return Err("device_id 为空".into());
    }
    com_init();
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let id = PCWSTR(wide.as_ptr());

    // 依版本尝试各接口布局；SetDefaultEndpoint 需对三个角色都生效
    macro_rules! try_switch {
        ($ty:ty, $ctx:expr) => {{
            let policy: Result<$ty, _> =
                unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, $ctx) };
            match policy {
                Ok(p) => unsafe {
                    p.SetDefaultEndpoint(id, ERole(0))
                        .and_then(|_| p.SetDefaultEndpoint(id, ERole(1)))
                        .and_then(|_| p.SetDefaultEndpoint(id, ERole(2)))
                },
                Err(e) => Err(e),
            }
        }};
    }

    /// 切换是否真实生效：立即读回 eConsole 默认设备比对。
    /// Win11 24H2 上部分激活方式 SetDefaultEndpoint 返回 Ok 但静默不生效
    /// （实测本机：返回 Ok 而默认设备纹丝不动），必须以读回结果为准。
    fn default_is(device_id: &str) -> bool {
        match list_devices("output") {
            Ok(list) => list
                .iter()
                .any(|d| d.is_default && d.id == device_id),
            Err(_) => false,
        }
    }

    // 组合顺序：Win11 24H2+ 的 PolicyConfig 移到了 COM 本地服务器，
    // 进程内激活（CLSCTX_ALL）会「成功但无效」，必须 CLSCTX_LOCAL_SERVER；
    // 旧系统则反之。每个组合切换后实测校验，无效就继续尝试下一个。
    let mut errs: Vec<String> = Vec::new();
    let combos: [(CLSCTX, u8); 8] = [
        (CLSCTX_LOCAL_SERVER, 0), // Win10 布局（24H2 主力）
        (CLSCTX_LOCAL_SERVER, 1), // 较新布局
        (CLSCTX_ALL, 0),          // Win10 布局（旧系统主力）
        (CLSCTX_ALL, 1),
        (CLSCTX_ALL, 2), // Vista 布局
        (CLSCTX_ALL, 3), // Win7 短布局
        (CLSCTX_LOCAL_SERVER, 2),
        (CLSCTX_LOCAL_SERVER, 3),
    ];
    for (ctx, layout) in combos {
        let r = match layout {
            0 => try_switch!(PolicyConfigWin10, ctx),
            1 => try_switch!(PolicyConfigNew, ctx),
            2 => try_switch!(PolicyConfigVista, ctx),
            _ => try_switch!(PolicyConfigLegacy, ctx),
        };
        if r.is_ok() && default_is(device_id) {
            return Ok(());
        }
        errs.push(format!("布局{layout}/ctx{:#x}：{}", ctx.0, match r {
            Ok(()) => "调用成功但未生效".to_string(),
            Err(e) => e.to_string(),
        }));
    }
    Err(format!("切换默认设备失败：{}", errs.join("；")))
}

#[tauri::command]
pub async fn set_default_audio_device(device_id: String) -> Result<(), String> {
    switch_default(&device_id)
}

fn open_endpoint(device_id: &str, kind: &str) -> Result<IAudioEndpointVolume, String> {
    let _ = flow_of(kind)?;
    let enumerator = enumerator()?;
    let dev: IMMDevice = unsafe { enumerator.GetDevice(&windows::core::HSTRING::from(device_id)) }
        .map_err(|e| e.to_string())?;
    unsafe { dev.Activate(CLSCTX_ALL, None) }.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_device_volume(device_id: String, kind: String) -> Result<EndpointVolume, String> {
    let vol = open_endpoint(&device_id, &kind)?;
    unsafe {
        let v = vol.GetMasterVolumeLevelScalar().map_err(|e| e.to_string())?;
        let m = vol.GetMute().map_err(|e| e.to_string())?;
        Ok(EndpointVolume {
            volume: (v * 100.0).round().clamp(0.0, 100.0) as u32,
            muted: m.as_bool(),
        })
    }
}

#[tauri::command]
pub async fn set_device_volume(
    device_id: String,
    kind: String,
    volume: Option<f64>,
    mute: Option<bool>,
) -> Result<(), String> {
    let vol = open_endpoint(&device_id, &kind)?;
    unsafe {
        if let Some(v) = volume {
            vol.SetMasterVolumeLevelScalar((v as f32 / 100.0).clamp(0.0, 1.0), std::ptr::null())
                .map_err(|e| e.to_string())?;
        }
        if let Some(m) = mute {
            vol.SetMute(m, std::ptr::null()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 当前系统默认输入设备（麦克风）的 id
pub fn default_device_id(kind: &str) -> Result<String, String> {
    let flow = flow_of(kind)?;
    let en = enumerator()?;
    let dev = unsafe { en.GetDefaultAudioEndpoint(flow, ERole(0)) }.map_err(|e| e.to_string())?;
    let id = device_id(&dev);
    if id.is_empty() {
        return Err("无法读取默认设备".into());
    }
    Ok(id)
}

pub fn get_default_muted(kind: &str) -> Result<bool, String> {
    let id = default_device_id(kind)?;
    let vol = open_endpoint(&id, kind)?;
    unsafe { vol.GetMute() }
        .map(|m| m.as_bool())
        .map_err(|e| e.to_string())
}

/// 切换默认麦克风静音，返回切换后的状态
pub fn toggle_default_input_mute() -> Result<bool, String> {
    let id = default_device_id("input")?;
    let vol = open_endpoint(&id, "input")?;
    unsafe {
        let m = vol.GetMute().map_err(|e| e.to_string())?;
        vol.SetMute(!m.as_bool(), std::ptr::null())
            .map_err(|e| e.to_string())?;
        Ok(!m.as_bool())
    }
}

#[tauri::command]
pub async fn get_mic_muted() -> Result<bool, String> {
    get_default_muted("input")
}

#[tauri::command]
pub async fn toggle_mic_mute() -> Result<bool, String> {
    toggle_default_input_mute()
}

/* ---------------- 音量混合器：按应用（音频会话）调节音量 ---------------- */

#[derive(Serialize)]
pub struct AudioSession {
    pub pid: u32,
    /// 显示名：exe 的 FileDescription，缺省用文件名；pid=0 为系统声音
    pub name: String,
    /// exe 完整路径（用于提取图标）；系统声音/进程已退出时为空串
    pub path: String,
    pub volume: u32, // 0-100
    pub muted: bool,
    /// 会话是否正在输出声音
    pub active: bool,
    /// true = 系统声音会话（无对应进程）
    pub system: bool,
}

fn default_render_device() -> Result<IMMDevice, String> {
    let en = enumerator()?;
    unsafe { en.GetDefaultAudioEndpoint(EDataFlow(0), ERole(0)) }.map_err(|e| e.to_string())
}

fn session_enumerator() -> Result<IAudioSessionEnumerator, String> {
    let dev = default_render_device()?;
    let mgr: IAudioSessionManager2 =
        unsafe { dev.Activate(CLSCTX_ALL, None) }.map_err(|e| e.to_string())?;
    unsafe { mgr.GetSessionEnumerator() }.map_err(|e| e.to_string())
}

/// 进程主模块完整路径；进程已退出或无权限时返回 None
fn process_image_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ret = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if ret.is_ok() && len > 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

/// 读 exe 版本资源里的 FileDescription（资源管理器/任务管理器显示的应用名）
fn file_description(path: &str) -> String {
    unsafe {
        let file = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&file, None);
        if size == 0 {
            return String::new();
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(&file, None, size, data.as_mut_ptr().cast()).is_err() {
            return String::new();
        }
        let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut block_len = 0u32;
        let trans_key = HSTRING::from("\\VarFileInfo\\Translation");
        if !VerQueryValueW(
            data.as_ptr().cast(),
            &trans_key,
            &mut block,
            &mut block_len,
        )
        .as_bool()
            || block_len < 4
        {
            return String::new();
        }
        let words = std::slice::from_raw_parts(block as *const u16, (block_len as usize) / 2);
        let query = HSTRING::from(format!(
            "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
            words[0], words[1]
        ));
        let mut buf: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut buf_len = 0u32;
        if VerQueryValueW(data.as_ptr().cast(), &query, &mut buf, &mut buf_len).as_bool()
            && buf_len > 0
        {
            let wide = std::slice::from_raw_parts(buf as *const u16, buf_len as usize);
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            String::from_utf16_lossy(&wide[..end]).trim().to_string()
        } else {
            String::new()
        }
    }
}

/// (显示名, exe 路径)；进程已退出返回 ("", "")
fn session_app_info(pid: u32) -> (String, String) {
    let path = process_image_path(pid).unwrap_or_default();
    if path.is_empty() {
        return (String::new(), String::new());
    }
    let desc = file_description(&path);
    let name = if desc.is_empty() {
        Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未知应用".into())
    } else {
        desc
    };
    (name, path)
}

#[tauri::command]
pub async fn list_audio_sessions() -> Result<Vec<AudioSession>, String> {
    com_init();
    let en = session_enumerator()?;
    unsafe {
        let count = en.GetCount().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let mut seen_pids = std::collections::HashSet::new(); // 同 pid 多会话只显示一条
        for i in 0..count {
            let ctl: IAudioSessionControl = en.GetSession(i).map_err(|e| e.to_string())?;
            let state = ctl.GetState().map_err(|e| e.to_string())?;
            if state == AudioSessionStateExpired {
                continue; // 进程已退出的残留会话
            }
            let c2: IAudioSessionControl2 = ctl.cast().map_err(|e| e.to_string())?;
            let pid = c2.GetProcessId().unwrap_or(0);
            if !seen_pids.insert(pid) {
                continue;
            }
            let system = pid == 0;
            let (mut name, path) = if system {
                ("系统声音".to_string(), String::new())
            } else {
                let (n, p) = session_app_info(pid);
                if n.is_empty() {
                    continue; // 进程已不可读（残留会话）
                }
                (n, p)
            };
            if name.is_empty() {
                name = "未知应用".into();
            }
            let vol: ISimpleAudioVolume = c2.cast().map_err(|e| e.to_string())?;
            let volume = vol
                .GetMasterVolume()
                .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u32)
                .unwrap_or(100);
            let muted = vol.GetMute().map(|m| m.as_bool()).unwrap_or(false);
            out.push(AudioSession {
                pid,
                name,
                path,
                volume,
                muted,
                active: state == AudioSessionStateActive,
                system,
            });
        }
        // 活动会话在前，系统声音置顶，同类按名称
        out.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then_with(|| b.system.cmp(&a.system))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }
}

/// 调节某个应用会话的音量/静音；pid=0 表示系统声音。
/// 同一进程可能有多个会话（少见），全部一并设置。
#[tauri::command]
pub async fn set_session_volume(pid: u32, volume: Option<f64>, mute: Option<bool>) -> Result<(), String> {
    com_init();
    let en = session_enumerator()?;
    unsafe {
        let count = en.GetCount().map_err(|e| e.to_string())?;
        let mut hit = false;
        for i in 0..count {
            let ctl: IAudioSessionControl = en.GetSession(i).map_err(|e| e.to_string())?;
            if ctl.GetState().map_err(|e| e.to_string())? == AudioSessionStateExpired {
                continue;
            }
            let c2: IAudioSessionControl2 = ctl.cast().map_err(|e| e.to_string())?;
            if c2.GetProcessId().unwrap_or(u32::MAX) != pid {
                continue;
            }
            let vol: ISimpleAudioVolume = c2.cast().map_err(|e| e.to_string())?;
            if let Some(v) = volume {
                vol.SetMasterVolume((v as f32 / 100.0).clamp(0.0, 1.0), std::ptr::null())
                    .map_err(|e| e.to_string())?;
            }
            if let Some(m) = mute {
                vol.SetMute(m, std::ptr::null()).map_err(|e| e.to_string())?;
            }
            hit = true;
        }
        if hit {
            Ok(())
        } else {
            Err("未找到该应用的音频会话".into())
        }
    }
}
