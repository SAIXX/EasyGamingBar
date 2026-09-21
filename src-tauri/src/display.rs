//! 真实显示调节能力（Windows）：
//! - 亮度：内屏走 WMI WmiMonitorBrightness；外接显示器走 DDC/CI（VCP 0x10）；
//!   两者皆不可用（LG 电视等无 DDC·CI 的设备）时回退 gamma 软件调光
//!   （OLED 像素自发光，压 gamma 等效降亮度；系统默认限制约 50%~100%）
//! - 分辨率 / 刷新率：EnumDisplaySettingsExW 枚举 + ChangeDisplaySettingsExW 切换
//! - HDR：直接绑定系统快捷键 Win+Alt+B（SendInput 注入），
//!   注入失败时回退 DisplayConfigSetDeviceInfo 直设
use serde::Serialize;
use std::collections::BTreeMap;
use tauri::WebviewWindow;
use windows::core::{BSTR, GUID, Interface, PCWSTR};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, DisplayConfigGetDeviceInfo,
    DisplayConfigSetDeviceInfo, GetCapabilitiesStringLength, GetDisplayConfigBufferSizes,
    GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, QueryDisplayConfig, SetVCPFeature,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, MC_VCP_CODE_TYPE, PHYSICAL_MONITOR, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{HANDLE, HWND, LUID, WIN32_ERROR};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, ChangeDisplaySettingsExW, EnumDisplaySettingsExW, GetMonitorInfoW,
    MonitorFromWindow, CDS_TYPE, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_FLAGS, ENUM_DISPLAY_SETTINGS_MODE, HMONITOR, MONITORINFO,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VariantClear, VARIANT};
use windows::Win32::UI::ColorSystem::SetDeviceGammaRamp;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_LWIN, VK_MENU,
};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE,
};

// WbemLocator 的 CLSID（windows-rs 未生成常量，取自 wbemcli.h）
const CLSID_WBEM_LOCATOR: GUID =
    GUID::from_u128(0x4590f811_1d3a_11d0_891f_00aa004b2e24);

#[derive(Serialize)]
pub struct DisplayResolution {
    pub width: u32,
    pub height: u32,
    /// 该分辨率下可用的刷新率，降序
    pub refreshes: Vec<u32>,
}

#[derive(Serialize)]
pub struct DisplayStatus {
    /// GDI 设备名（\\.\DISPLAY1 形态）
    pub device: String,
    pub brightness_supported: bool,
    /// 0-100；读不到当前值时为 null（仍可调节）
    pub brightness: Option<u32>,
    /// 可用分辨率列表，按像素面积降序
    pub resolutions: Vec<DisplayResolution>,
    pub current_width: u32,
    pub current_height: u32,
    pub current_refresh: u32,
    /// HDR 开/关；None = 状态读取不可用（仅影响显示，不影响开关操作）
    pub hdr_enabled: Option<bool>,
}

/* ---------- 基础工具 ---------- */

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn com_init() {
    // 线程池线程可能已被初始化为 STA，忽略该错误即可（同 audio.rs）
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// 面板窗口所在显示器；取不到时退回主显示器
fn active_monitor(window: &WebviewWindow) -> HMONITOR {
    if let Ok(hwnd) = window.hwnd() {
        let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        if !mon.is_invalid() {
            return mon;
        }
    }
    unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTONEAREST) }
}

/// 显示器的 GDI 设备名（\\.\DISPLAY1），ChangeDisplaySettingsEx / DisplayConfig 均用它定位
pub fn monitor_device_name(monitor: HMONITOR) -> Result<String, String> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        szDevice: [0; 32],
    };
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if !ok.as_bool() {
        return Err("无法读取显示器信息".into());
    }
    Ok(from_wide(&info.szDevice))
}

/* ---------- 亮度：WMI（内屏） ---------- */

fn wmi_services() -> Result<IWbemServices, String> {
    com_init();
    unsafe {
        let locator: IWbemLocator =
            CoCreateInstance(&CLSID_WBEM_LOCATOR, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| e.to_string())?;
        let empty = BSTR::new();
        locator
            .ConnectServer(
                &BSTR::from("ROOT\\WMI"),
                &empty,
                &empty,
                &empty,
                0,
                &empty,
                None,
            )
            .map_err(|e| e.to_string())
    }
}

fn wmi_query(svc: &IWbemServices, class: &str) -> Result<IEnumWbemClassObject, String> {
    let sql = format!("SELECT * FROM {class}");
    unsafe {
        svc.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(&sql),
            WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0),
            None,
        )
        .map_err(|e| e.to_string())
    }
}

/// 取枚举中的下一个实例，取完返回 None
fn wmi_next(en: &IEnumWbemClassObject) -> Option<IWbemClassObject> {
    let mut objs: [Option<IWbemClassObject>; 1] = [None];
    let mut returned = 0u32;
    let hr = unsafe { en.Next(-1, &mut objs, &mut returned) };
    if hr.is_ok() && returned == 1 {
        objs[0].take()
    } else {
        None
    }
}

fn obj_string(obj: &IWbemClassObject, prop: &str) -> Option<String> {
    let mut val = VARIANT::default();
    let name = BSTR::from(prop);
    if unsafe { obj.Get(&name, 0, &mut val, None, None) }.is_err() {
        return None;
    }
    let s = val.to_string();
    unsafe {
        let _ = VariantClear(&mut val);
    }
    Some(s)
}

fn obj_u32(obj: &IWbemClassObject, prop: &str) -> Option<u32> {
    obj_string(obj, prop)?.trim().parse().ok()
}

/// WmiMonitorBrightness：读内屏当前亮度
pub fn wmi_get_brightness() -> Result<Option<u32>, String> {
    let svc = wmi_services()?;
    let en = wmi_query(&svc, "WmiMonitorBrightness")?;
    Ok(wmi_next(&en).and_then(|obj| obj_u32(&obj, "CurrentBrightness")))
}

/// WmiSetBrightness(Timeout=0, Brightness=value)：多实例（含坞站残留）时逐个尝试
pub fn wmi_set_brightness(value: u32) -> Result<bool, String> {
    let svc = wmi_services()?;
    let en = wmi_query(&svc, "WmiMonitorBrightnessMethods")?;
    while let Some(inst) = wmi_next(&en) {
        let Some(path) = obj_string(&inst, "__PATH") else {
            continue;
        };
        unsafe {
            // 输入参数签名取自类定义
            let mut class_obj: Option<IWbemClassObject> = None;
            if svc
                .GetObject(
                    &BSTR::from("WmiMonitorBrightnessMethods"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    Some(&mut class_obj),
                    None,
                )
                .is_err()
            {
                continue;
            }
            let Some(class_obj) = class_obj else { continue };
            let mut sig_in: Option<IWbemClassObject> = None;
            if class_obj
                .GetMethod(
                    &BSTR::from("WmiSetBrightness"),
                    0,
                    &mut sig_in,
                    std::ptr::null_mut(),
                )
                .is_err()
            {
                continue;
            }
            let Some(sig_in) = sig_in else { continue };
            let Ok(input) = sig_in.SpawnInstance(0) else {
                continue;
            };
            let timeout = VARIANT::from(0u32);
            if input.Put(&BSTR::from("Timeout"), 0, &timeout, 0).is_err() {
                continue;
            }
            let level = VARIANT::from(value);
            if input.Put(&BSTR::from("Brightness"), 0, &level, 0).is_err() {
                continue;
            }
            if svc
                .ExecMethod(
                    &BSTR::from(&path),
                    &BSTR::from("WmiSetBrightness"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &input,
                    None,
                    None,
                )
                .is_ok()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/* ---------- 亮度：DDC/CI（外接显示器） ---------- */

const VCP_BRIGHTNESS: u8 = 0x10;

/// 存在物理显示器句柄即视为亮度可调。
/// 与 Twinkle Tray 一致：不少显示器读不了 VCP 亮度，但写入仍然有效
fn has_physical_monitor(monitor: HMONITOR) -> bool {
    unsafe {
        let mut count = 0u32;
        GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).is_ok() && count > 0
    }
}

/// DDC/CI 能力探测：caps 长度为 0 说明显示器完全不响应 DDC/CI
/// （LG 电视等）。此时 SetVCPFeature 会返回"假成功"，实际无效。
fn ddc_caps_available(handle: HANDLE) -> bool {
    unsafe {
        let mut len = 0u32;
        GetCapabilitiesStringLength(handle, &mut len) == 0 && len > 0
    }
}

/// 对显示器第一个物理句柄执行 VCP 0x10 读/写。
/// caps 为空的设备直接放弃（假成功不可信）；caps 有但读失败仍允许写，
/// 此时按 0-100 满量程直接下发（node-ddcci 同款兜底）
pub fn ddc_brightness(monitor: HMONITOR, set: Option<u32>) -> Result<Option<u32>, String> {
    unsafe {
        let mut count = 0u32;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).is_err() || count == 0 {
            return Ok(None);
        }
        let mut arr = vec![PHYSICAL_MONITOR::default(); count as usize];
        if GetPhysicalMonitorsFromHMONITOR(monitor, &mut arr).is_err() {
            return Ok(None);
        }
        let handle = arr[0].hPhysicalMonitor;
        if !ddc_caps_available(handle) {
            for m in &arr {
                let _ = DestroyPhysicalMonitor(m.hPhysicalMonitor);
            }
            return Ok(None);
        }
        let mut value_type = MC_VCP_CODE_TYPE(0);
        let mut cur = 0u32;
        let mut max = 0u32;
        let read_ok = GetVCPFeatureAndVCPFeatureReply(
            handle,
            VCP_BRIGHTNESS,
            Some(&mut value_type),
            &mut cur,
            Some(&mut max),
        ) != 0
            && max > 0;
        let mut result = None;
        match set {
            Some(v) => {
                let target = if read_ok {
                    ((v.clamp(0, 100) as u64 * max as u64) / 100).min(max as u64) as u32
                } else {
                    v.clamp(0, 100) // 读不到量程，假定 0-100
                };
                if SetVCPFeature(handle, VCP_BRIGHTNESS, target) != 0 {
                    result = Some(v);
                }
            }
            None => {
                if read_ok {
                    result = Some(((cur as u64 * 100) / max as u64).min(100) as u32);
                }
            }
        }
        for m in &arr {
            let _ = DestroyPhysicalMonitor(m.hPhysicalMonitor);
        }
        Ok(result)
    }
}

/* ---------- 亮度回退：gamma 软件调光（无 DDC·CI 的电视 / OLED） ---------- */

/// OLED 像素自发光，压 gamma 等效真实降亮度。
/// Windows 默认限制调光幅度为 0.5x-10x（注册表 GdiIcmGammaRange=256 可解锁），
/// 故有效范围为 100%（不干预）~50%（最暗）；HDR 开启时不生效。
fn gamma_brightness(device: &str, value: u32) -> Result<(), String> {
    let name = to_wide(device);
    unsafe {
        let hdc = CreateDCW(PCWSTR(name.as_ptr()), PCWSTR::null(), PCWSTR::null(), None);
        if hdc.is_invalid() {
            return Err("无法打开显示设备".into());
        }
        let f = (value.clamp(0, 100) as f64 / 100.0).clamp(0.5, 1.0);
        let mut ramp = [0u16; 768];
        for i in 0..256usize {
            let v = ((i as f64 / 255.0) * 65535.0 * f).round().clamp(0.0, 65535.0) as u16;
            ramp[i] = v;
            ramp[256 + i] = v;
            ramp[512 + i] = v;
        }
        let ok = SetDeviceGammaRamp(hdc, ramp.as_ptr() as *const _);
        let _ = DeleteDC(hdc);
        if !ok.as_bool() {
            return Err("软件调光被系统拒绝".into());
        }
        Ok(())
    }
}

/* ---------- 分辨率 / 刷新率 ---------- */

const MIN_REFRESH: u32 = 30; // 过滤 1Hz 等 TV 兼容模式

pub fn enum_modes(device: &str) -> Result<Vec<DisplayResolution>, String> {
    let name = to_wide(device);
    let mut groups: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    unsafe {
        let mut i = 0u32;
        loop {
            let mut dm: DEVMODEW = std::mem::zeroed();
            dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if !EnumDisplaySettingsExW(
                PCWSTR(name.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
            .as_bool()
            {
                break;
            }
            i += 1;
            if dm.dmBitsPerPel == 32 && dm.dmDisplayFrequency >= MIN_REFRESH {
                groups
                    .entry((dm.dmPelsWidth, dm.dmPelsHeight))
                    .or_default()
                    .push(dm.dmDisplayFrequency);
            }
        }
    }
    let mut list: Vec<DisplayResolution> = groups
        .into_iter()
        .map(|((width, height), mut hz)| {
            hz.sort_unstable();
            hz.dedup();
            hz.reverse();
            DisplayResolution {
                width,
                height,
                refreshes: hz,
            }
        })
        .collect();
    list.sort_by(|a, b| {
        (b.width as u64 * b.height as u64).cmp(&(a.width as u64 * a.height as u64))
    });
    Ok(list)
}

pub fn current_mode(device: &str) -> Result<(u32, u32, u32), String> {
    let name = to_wide(device);
    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let ok = unsafe {
        EnumDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    };
    if !ok.as_bool() {
        return Err("无法读取当前显示模式".into());
    }
    Ok((dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency))
}

/* ---------- HDR（直接绑定 Win+Alt+B 系统快捷键，不做支持预判） ---------- */

// GET_ADVANCED_COLOR 匿名联合体的位定义（windisplayapi.h）
const ADV_COLOR_ENABLED: u32 = 0x1;

/// 定位当前显示器对应的 DisplayConfig 路径，并尽力读取 HDR 当前状态。
/// GDI 名匹配不上时退回第一个活动路径；状态读取失败只影响显示，不影响开关操作。
fn hdr_target_info(device: &str) -> Result<(LUID, u32, Option<bool>), String> {
    unsafe {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let err =
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count);
        if err != WIN32_ERROR(0) {
            return Err(format!("GetDisplayConfigBufferSizes 失败：{}", err.0));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let err = QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        );
        if err != WIN32_ERROR(0) {
            return Err(format!("QueryDisplayConfig 失败：{}", err.0));
        }
        let mut fallback: Option<(LUID, u32, Option<bool>)> = None;
        for path in &paths[..path_count as usize] {
            // 源 GDI 设备名尽力匹配当前显示器
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = path.sourceInfo.adapterId;
            src.header.id = path.sourceInfo.id;
            let matched = DisplayConfigGetDeviceInfo(
                &mut src as *mut _ as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER,
            ) == 0
                && from_wide(&src.viewGdiDeviceName) == device;

            let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            info.header.size = std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            info.header.adapterId = path.targetInfo.adapterId;
            info.header.id = path.targetInfo.id;
            let enabled = if DisplayConfigGetDeviceInfo(
                &mut info as *mut _ as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER,
            ) == 0
            {
                Some(info.Anonymous.value & ADV_COLOR_ENABLED != 0)
            } else {
                None
            };

            if matched {
                return Ok((path.targetInfo.adapterId, path.targetInfo.id, enabled));
            }
            if fallback.is_none() {
                fallback = Some((path.targetInfo.adapterId, path.targetInfo.id, enabled));
            }
        }
        fallback.ok_or_else(|| "没有活动的显示路径".into())
    }
}

/// DXGI 侧核验：IDXGIOutput6 色彩空间为 G2084（ST.2084 HDR10）= HDR 激活。
/// 与 CCD AdvancedColorInfo 相互独立：CCD 查询失败或个别驱动上不可靠时以它兜底。
fn hdr_active_dxgi(device: &str) -> Option<bool> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut a = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0u32;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            o += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            let len = desc
                .DeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.DeviceName.len());
            let name = String::from_utf16_lossy(&desc.DeviceName[..len]);
            if name != device {
                continue;
            }
            let Ok(out6) = output.cast::<IDXGIOutput6>() else {
                return None;
            };
            let d1 = unsafe { out6.GetDesc1() }.ok()?;
            // G2084（ST.2084 HDR10）色彩空间 = HDR 当前激活
            return Some(d1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
        }
    }
    None
}

/// 仅供自测示例：读取第一个活动路径的 HDR 状态
/// 返回 (CCD AdvancedColorEnabled, DXGI G2084 色彩空间)
#[doc(hidden)]
pub fn hdr_state_probe(device: &str) -> (Option<bool>, Option<bool>) {
    let ccd = hdr_target_info(device).ok().and_then(|(_, _, e)| e);
    (ccd, hdr_active_dxgi(device))
}

/// 轻量 HDR 状态探测（悬浮条快捷开关低频轮询用；不做亮度/分辨率等重查询）。
/// 返回 (CCD AdvancedColorEnabled, DXGI G2084 色彩空间)，DXGI 优先采信。
#[tauri::command]
pub fn hdr_probe(window: tauri::WebviewWindow) -> (Option<bool>, Option<bool>) {
    // 匹配调用方窗口所在显示器；匹配失败回退第一个活动路径
    let monitor = active_monitor(&window);
    let device = monitor_device_name(monitor).unwrap_or_default();
    hdr_state_probe(&device)
}

/// DisplayConfig 直设（热键注入失败时的回退路径）
fn set_hdr_target(device: &str, enabled: bool) -> Result<(), String> {
    let (adapter, id, _) = hdr_target_info(device)?;
    let mut set = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE::default();
    set.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE;
    set.header.size = std::mem::size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32;
    set.header.adapterId = adapter;
    set.header.id = id;
    set.Anonymous.value = if enabled { ADV_COLOR_ENABLED } else { 0 };
    let err = unsafe {
        DisplayConfigSetDeviceInfo(&mut set as *mut _ as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER)
    };
    if err != 0 {
        return Err(format!("HDR 切换失败：{err}"));
    }
    Ok(())
}

/* ---------- Tauri 命令 ---------- */

#[tauri::command]
pub async fn get_display_status(window: WebviewWindow) -> Result<DisplayStatus, String> {
    let monitor = active_monitor(&window);
    let device = monitor_device_name(monitor)?;

    // 亮度：优先 WMI（内屏），失败退 DDC/CI 读（外接）
    let mut brightness = wmi_get_brightness().unwrap_or(None);
    if brightness.is_none() {
        brightness = ddc_brightness(monitor, None).unwrap_or(None);
    }
    // 有物理显示器即视为可调（Twinkle Tray 策略：读不了也能写）
    let brightness_supported = brightness.is_some() || has_physical_monitor(monitor);

    let resolutions = enum_modes(&device)?;
    let (current_width, current_height, current_refresh) = current_mode(&device)?;

    let mut hdr_enabled = match hdr_target_info(&device) {
        Ok((_, _, enabled)) => enabled,
        Err(_) => None,
    };
    // DXGI 色彩空间核验：G2084 = HDR 实际激活，可信度高于 CCD 位
    if let Some(dxgi) = hdr_active_dxgi(&device) {
        hdr_enabled = Some(dxgi);
    }

    Ok(DisplayStatus {
        brightness_supported,
        brightness,
        device,
        resolutions,
        current_width,
        current_height,
        current_refresh,
        hdr_enabled,
    })
}

#[tauri::command]
pub async fn set_brightness(window: WebviewWindow, value: u32) -> Result<(), String> {
    if value > 100 {
        return Err("亮度取值须在 0-100".into());
    }
    let monitor = active_monitor(&window);
    // 1) 内屏 WMI（出错不阻断）
    if matches!(wmi_set_brightness(value), Ok(true)) {
        return Ok(());
    }
    // 2) 外接 DDC/CI（caps 为空的电视已在内部过滤，不会假成功）
    if ddc_brightness(monitor, Some(value))?.is_some() {
        return Ok(());
    }
    // 3) 回退：gamma 软件调光（OLED / 电视）
    let device = monitor_device_name(monitor)?;
    gamma_brightness(&device, value)
}

#[tauri::command]
pub async fn set_display_mode(
    window: WebviewWindow,
    width: u32,
    height: u32,
    refresh: u32,
) -> Result<(), String> {
    let monitor = active_monitor(&window);
    let device = monitor_device_name(monitor)?;

    // 仅接受枚举到的真实模式，防止应用屏外/黑屏参数
    let valid = enum_modes(&device)?.iter().any(|m| {
        m.width == width && m.height == height && m.refreshes.contains(&refresh)
    });
    if !valid {
        return Err(format!("{width}×{height}@{refresh}Hz 不是可用显示模式"));
    }

    let name = to_wide(&device);
    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    dm.dmBitsPerPel = 32;
    dm.dmPelsWidth = width;
    dm.dmPelsHeight = height;
    dm.dmDisplayFrequency = refresh;
    dm.dmFields = DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    let ret = unsafe {
        ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_TYPE(0), None)
    };
    if ret != DISP_CHANGE_SUCCESSFUL {
        return Err(format!("切换显示模式失败（代码 {}）", ret.0));
    }
    Ok(())
}

#[tauri::command]
pub async fn set_hdr(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    let monitor = active_monitor(&window);
    let device = monitor_device_name(monitor)?;
    set_hdr_target(&device, enabled)
}

/// 发送 Win+Alt+B（Xbox Game Bar 的 HDR 开关，与手按快捷键完全等效）。
/// 比直接调 DisplayConfig 更稳：由系统自己处理 HDR 的完整切换流程。
#[tauri::command]
pub async fn toggle_hdr_hotkey() -> Result<(), String> {
    let vk_b = VIRTUAL_KEY(0x42); // 'B'
    let press = |vk: VIRTUAL_KEY, up: bool| {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    };
    let inputs = [
        press(VK_LWIN, false),
        press(VK_MENU, false),
        press(vk_b, false),
        press(vk_b, true),
        press(VK_MENU, true),
        press(VK_LWIN, true),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(format!("快捷键注入失败（{sent}/{}）", inputs.len()));
    }
    Ok(())
}
