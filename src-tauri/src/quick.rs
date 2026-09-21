//! 快捷指令组件的系统级动作：
//! - 截图：隐藏本软件全部窗口 → 注入 Win+Shift+S 由用户框选 → 框选结果经剪贴板
//!   存为 PNG 到设置的保存目录（settings.saveDir，未设置时为系统图片库）→ 恢复窗口
//! - 返回主页（显示桌面）：注入 Win+D
//! - 关机：Windows 标准 `shutdown /s /t 0`（/s 关机，/t 0 立即执行，与开始菜单关机同路径）
//! - 强制关闭游戏：向前台窗口投递 WM_SYSCOMMAND/SC_CLOSE —— 即按下 Alt+F4 时
//!   系统实际走的关闭路径；目标优先取当前前台窗口，若前台是本应用自身
//!   （悬浮条/组件面板抢了焦点），则取 Z 序最高的非本应用可见窗口（被遮挡的游戏）
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber, IsClipboardFormatAvailable};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, ShowWindow,
    GWL_EXSTYLE, SC_CLOSE, SW_HIDE, SW_SHOWNA, WM_CLOSE, WM_SYSCOMMAND, WS_EX_TOOLWINDOW,
};

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 按住 mods 中的修饰键 → 敲 key → 逆序松开（与手按组合键的顺序一致）
fn send_combo(mods: &[VIRTUAL_KEY], key: VIRTUAL_KEY) -> Result<(), String> {
    let mut inputs: Vec<INPUT> = mods.iter().map(|vk| key_input(*vk, false)).collect();
    inputs.push(key_input(key, false));
    inputs.push(key_input(key, true));
    inputs.extend(mods.iter().rev().map(|vk| key_input(*vk, true)));
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(format!("快捷键注入失败（{sent}/{}）", inputs.len()));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct ScreenshotResult {
    pub saved: bool,
    /// 保存成功时的 PNG 绝对路径
    pub path: Option<String>,
}

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
    GetMonitorInfoW, MonitorFromPoint, ReleaseDC, SelectObject, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_FROM_FLAGS, HPALETTE, HBITMAP, SRCCOPY,
};
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromHBITMAP, GdipDisposeImage, GdipSaveImageToFile, GdiplusStartup,
    GdiplusStartupInput,
};
use windows::Win32::Graphics::GdiPlus;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

/// 磁盘配置里的 settings.saveDir（设置中心「文件保存地址」）。
/// 以磁盘为唯一权威来源：快捷抽屉持有的前端副本可能尚未加载完或已过期
/// （曾导致截图落进默认图片库而不是用户设置的文件夹）。
fn config_save_dir(app: &AppHandle) -> Option<String> {
    let raw = std::fs::read_to_string(
        app.path()
            .app_data_dir()
            .ok()?
            .join("config.json"),
    )
    .ok()?;
    let cfg: Value = serde_json::from_str(&raw).ok()?;
    let dir = cfg.get("settings")?.get("saveDir")?.as_str()?;
    if dir.trim().is_empty() {
        None
    } else {
        Some(dir.to_string())
    }
}

fn resolve_screenshot_dir(save_dir: &str, app: &AppHandle) -> PathBuf {
    // ① 磁盘配置 → ② 前端传值兜底 → ③ 系统图片库（与设置中心默认一致）
    if let Some(dir) = config_save_dir(app) {
        return PathBuf::from(dir);
    }
    if !save_dir.trim().is_empty() {
        PathBuf::from(save_dir)
    } else {
        dirs::picture_dir().unwrap_or_else(|| PathBuf::from("."))
    }
}

struct HideCtx {
    me: u32,
    hidden: Vec<HWND>,
}

/// 隐藏本进程的所有可见窗口（截图内容因此不含本软件 UI），记录待恢复
unsafe extern "system" fn hide_own(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut HideCtx);
    if window_pid(hwnd) == ctx.me && IsWindowVisible(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_HIDE);
        ctx.hidden.push(hwnd);
    }
    BOOL(1)
}

fn restore_windows(hidden: &[HWND]) {
    for &h in hidden {
        if unsafe { IsWindow(Some(h)) }.as_bool() {
            unsafe { let _ = ShowWindow(h, SW_SHOWNA); }; // 不抢回焦点
        }
    }
}

fn save_clipboard_image(dir: &str) -> Result<String, String> {
    // 目录以单引号字符串注入脚本，配对单引号转义；文件名仅含 ASCII，stdout 无编码问题
    let esc_dir = dir.replace('\'', "''");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $img=$null; try{{ $img=[System.Windows.Forms.Clipboard]::GetImage() }}catch{{}}; \
         if($null -eq $img){{ exit 1 }}; \
         $name='screenshot_' + (Get-Date -Format 'yyyyMMdd_HHmmss') + '.png'; \
         $out=Join-Path '{dir}' $name; \
         $img.Save($out,[System.Drawing.Imaging.ImageFormat]::Png); \
         Write-Output $name",
        dir = esc_dir
    );
    let output = crate::spawn_no_window("powershell")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", &script])
        .output()
        .map_err(|e| format!("启动图像保存失败：{e}"))?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && !name.is_empty() {
        Ok(PathBuf::from(dir).join(name).to_string_lossy().into_owned())
    } else {
        Err("剪贴板中暂无可保存的图片".into())
    }
}

/// 光标所在显示器整屏截取为 HBITMAP（物理像素）
unsafe fn capture_monitor_bitmap() -> Result<(HBITMAP, i32, i32), String> {
    let mut pt = POINT::default();
    GetCursorPos(&mut pt).map_err(|_| "获取光标位置失败".to_string())?;
    let hmon = MonitorFromPoint(pt, MONITOR_FROM_FLAGS(MONITOR_DEFAULTTONEAREST.0));
    let mut mi = MONITORINFO::default();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
        return Err("获取显示器信息失败".into());
    }
    let x = mi.rcMonitor.left;
    let y = mi.rcMonitor.top;
    let w = mi.rcMonitor.right - mi.rcMonitor.left;
    let h = mi.rcMonitor.bottom - mi.rcMonitor.top;
    if w <= 0 || h <= 0 {
        return Err("显示器尺寸无效".into());
    }

    let hdc_screen = GetDC(None);
    if hdc_screen.is_invalid() {
        return Err("获取屏幕 DC 失败".into());
    }
    let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
    if hdc_mem.is_invalid() {
        ReleaseDC(None, hdc_screen);
        return Err("创建内存 DC 失败".into());
    }
    let hbm = CreateCompatibleBitmap(hdc_screen, w, h);
    let old = SelectObject(hdc_mem, hbm.into());
    let blit = BitBlt(hdc_mem, 0, 0, w, h, Some(hdc_screen), x, y, SRCCOPY);
    ReleaseDC(None, hdc_screen);
    if blit.is_err() {
        SelectObject(hdc_mem, old);
        DeleteObject(hbm.into());
        DeleteDC(hdc_mem);
        return Err("抓屏失败".into());
    }
    SelectObject(hdc_mem, old);
    DeleteDC(hdc_mem);
    Ok((hbm, w, h))
}

/// HBITMAP → PNG 落盘（GDI+，PNG 编码器 CLSID 为系统固定值）；返回文件名
/// 全屏截取 → PNG 落盘；返回文件名（examples/shot_test.rs 端到端验证用）
pub fn capture_screen_png(dir: &str) -> Result<String, String> {
    let (hbm, _w, _h) = unsafe { capture_monitor_bitmap()? };
    let res = unsafe { save_hbitmap_png(dir, hbm) };
    unsafe {
        let _ = DeleteObject(hbm.into());
    }
    res
}

/// GDI+ 进程级会话：首次截图时启动一次，之后常驻（不再每次启动/关闭——
/// 反复初始化偶发后续 GdipCreateBitmapFromHBITMAP 失败；进程退出由系统回收）。
/// 初始化失败不缓存，下次截图自动重试。
static GDIPLUS: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

fn ensure_gdiplus() -> Result<(), String> {
    let mut slot = GDIPLUS.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Ok(());
    }
    let mut token = 0usize;
    // GDI+ 只接受 GdiplusVersion=1；windows crate 的 default() 是全零结构体，
    // 传 0 会返回 UnsupportedGdiplusVersion(17)，即「GDI+ 初始化失败」
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..Default::default()
    };
    if unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) }.0 != 0 {
        return Err("GDI+ 初始化失败".into());
    }
    *slot = Some(token);
    Ok(())
}

unsafe fn save_hbitmap_png(dir: &str, hbm: HBITMAP) -> Result<String, String> {
    ensure_gdiplus()?;
    let res = (|| -> Result<String, String> {
        let mut bitmap: *mut GdiPlus::GpBitmap = std::ptr::null_mut();
        let st = GdipCreateBitmapFromHBITMAP(hbm, HPALETTE::default(), &mut bitmap);
        if st.0 != 0 {
            return Err(format!("位图转换失败（GDI+ status {}）", st.0));
        }
        let image = bitmap as *mut GdiPlus::GpImage;
        let st = GetLocalTime();
        let name = format!(
            "screenshot_{:04}{:02}{:02}_{:02}{:02}{:02}.png",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        );
        let path: windows::core::HSTRING =
            std::path::Path::new(dir).join(&name).to_string_lossy().to_string().into();
        // PNG 编码器 CLSID 为系统固定值
        let save = GdipSaveImageToFile(
            image,
            windows::core::PCWSTR(path.as_ptr()),
            &windows::core::GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e),
            std::ptr::null(),
        );
        GdipDisposeImage(image);
        if save.0 != 0 {
            return Err(format!("PNG 保存失败（GDI+ status {}）", save.0));
        }
        Ok(name)
    })();
    res
}

/// 全屏直截：隐藏本软件 UI → 光标所在显示器整屏截取为 PNG → 恢复窗口。
/// 不弹系统截图框选，无需划区。
#[tauri::command]
pub async fn quick_screenshot_full(
    app: tauri::AppHandle,
    save_dir: String,
) -> Result<ScreenshotResult, String> {
    let dir = resolve_screenshot_dir(&save_dir, &app);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建截图目录失败：{e}"))?;

    // 1. 隐藏本软件全部可见窗口（含悬浮条/面板/FPS 悬浮窗），保证画面干净
    let mut ctx = HideCtx { me: std::process::id(), hidden: Vec::new() };
    unsafe {
        let _ = EnumWindows(
            Some(hide_own),
            LPARAM(&mut ctx as *mut HideCtx as isize),
        );
    }
    std::thread::sleep(Duration::from_millis(350)); // 等桌面重绘

    let result = capture_screen_png(dir.to_string_lossy().as_ref());

    restore_windows(&ctx.hidden);
    match result {
        Ok(name) => Ok(ScreenshotResult {
            saved: true,
            path: Some(std::path::PathBuf::from(&dir).join(name).to_string_lossy().into_owned()),
        }),
        Err(e) => Err(e),
    }
}

/// BGRA 像素（DXGI 桌面复制输出）→ 缩到 ≤1600 宽 → JPEG(q85) 落盘；返回文件名字节。
/// 4K 游戏截图 PNG 有 15~25MB，会撑爆大模型接口的请求体上限（Kimi 10MB）；
/// 压成 1600px JPEG 后一般 <500KB，识别任务名的清晰度足够。
fn save_pixels_jpeg(dir: &str, pixels: &[u8], w: i32, h: i32) -> Result<String, String> {
    ensure_gdiplus()?;
    let st = unsafe { GetLocalTime() };
    let name = format!(
        "guide_{:04}{:02}{:02}_{:02}{:02}{:02}.jpg",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    );
    let path: windows::core::HSTRING =
        std::path::Path::new(dir).join(&name).to_string_lossy().to_string().into();
    let res: Result<(), String> = unsafe {
        let mut bitmap: *mut GdiPlus::GpBitmap = std::ptr::null_mut();
        // stride = 行字节数（BGRA 每像素 4 字节）；scan0 指向调用方持有的像素缓冲
        let st0 = GdiPlus::GdipCreateBitmapFromScan0(
            w,
            h,
            w * 4,
            0x26200A, // PixelFormat32bppARGB（windows crate 未导出常量，取 SDK 固定值）
            Some(pixels.as_ptr() as *const u8),
            &mut bitmap,
        );
        if st0.0 != 0 {
            return Err(format!("位图构建失败（GDI+ status {}）", st0.0));
        }
        let image = bitmap as *mut GdiPlus::GpImage;
        // 缩略图（等比例，宽上限 1600）
        let (tw, th) = if w > 1600 {
            (1600, ((h as f64) * 1600.0 / (w as f64)).round() as u32)
        } else {
            (w as u32, h as u32)
        };
        let mut thumb: *mut GdiPlus::GpImage = std::ptr::null_mut();
        let st1 = GdiPlus::GdipGetImageThumbnail(
            image,
            tw,
            th,
            &mut thumb,
            0, // ImageGetNativeImageCallback：None
            std::ptr::null_mut(),
        );
        GdipDisposeImage(image);
        if st1.0 != 0 {
            return Err(format!("缩放失败（GDI+ status {}）", st1.0));
        }
        // JPEG 编码器 + 质量参数
        let mut quality: u32 = 85;
        let mut params = GdiPlus::EncoderParameters {
            Count: 1,
            ..Default::default()
        };
        params.Parameter[0] = GdiPlus::EncoderParameter {
            Guid: windows::core::GUID::from_u128(0x1d5be4b5_fa4a_452d_9cdd_5db35105e7eb), // 图像质量
            NumberOfValues: 1,
            Type: GdiPlus::EncoderParameterValueTypeLong.0 as u32,
            Value: &mut quality as *mut u32 as *mut std::ffi::c_void,
        };
        let save = GdipSaveImageToFile(
            thumb,
            &path,
            // JPEG 编码器 CLSID（系统固定值）
            &windows::core::GUID::from_u128(0x557cf402_1a04_11d3_9a73_0000f81ef32e),
            &params,
        );
        GdipDisposeImage(thumb);
        if save.0 != 0 {
            return Err(format!("JPEG 保存失败（GDI+ status {}）", save.0));
        }
        Ok(())
    };
    res?;
    Ok(name)
}

/// 干净整屏截屏 → JPEG 字节（攻略助手上传用）。
/// 优先 DXGI Desktop Duplication：独占全屏游戏的画面 GDI BitBlt 只能截到黑屏，
/// 桌面复制则能拿到真实游戏帧（OBS 同源方案）；DXGI 失败回退 GDI 文件链路。
/// 全程隐藏本软件窗口，返回的字节即最终图片。
pub fn capture_screen_clean_bytes() -> Result<Vec<u8>, String> {
    let dir = std::env::temp_dir().join("egb_guide");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    // 截屏前把光标挪到屏幕左下角：游戏菜单里「鼠标悬停」的条目会临时变亮，
    // 而用户要点悬浮条上的「找攻略」，光标必然正压在任务列表上——模型会把
    // 悬停项误认成当前任务。挪到同屏角落（不影响按光标选显示器的逻辑），
    // 等菜单重绘后悬停高亮自然消退；截完放回原位。
    let mut orig = POINT::default();
    let mut parked = false;
    unsafe {
        if GetCursorPos(&mut orig).is_ok() {
            let hmon = MonitorFromPoint(orig, MONITOR_FROM_FLAGS(MONITOR_DEFAULTTONEAREST.0));
            let mut mi = MONITORINFO::default();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                // 左下角内缩 8px：避开任务栏 Start 按钮热区
                SetCursorPos(mi.rcMonitor.left + 8, mi.rcMonitor.bottom - 8);
                parked = true;
            }
        }
    }
    let mut ctx = HideCtx { me: std::process::id(), hidden: Vec::new() };
    unsafe {
        let _ = EnumWindows(
            Some(hide_own),
            LPARAM(&mut ctx as *mut HideCtx as isize),
        );
    }
    std::thread::sleep(Duration::from_millis(450)); // 等窗口隐藏 + 悬停高亮消退重绘
    let result = (|| -> Result<Vec<u8>, String> {
        match crate::capture::capture_cursor_monitor() {
            Ok((w, h, pixels, _pitch)) => {
                let name = save_pixels_jpeg(&dir.to_string_lossy(), &pixels, w, h)?;
                let bytes = std::fs::read(dir.join(&name)).map_err(|e| format!("读取截图失败：{e}"))?;
                let _ = std::fs::remove_file(dir.join(name));
                Ok(bytes)
            }
            Err(e) => {
                log::warn!("DXGI 截屏失败（{e}），回退 GDI BitBlt");
                let name = capture_screen_png(&dir.to_string_lossy())?;
                let bytes = std::fs::read(dir.join(&name)).map_err(|e| format!("读取截图失败：{e}"))?;
                let _ = std::fs::remove_file(dir.join(name));
                Ok(bytes)
            }
        }
    })();
    restore_windows(&ctx.hidden);
    if parked {
        unsafe { SetCursorPos(orig.x, orig.y) }; // 光标回原位（原本就在悬浮条按钮上）
    }
    result
}

/// 截图：隐藏本软件 UI → 注入 Win+Shift+S（用户框选，图自动进剪贴板）→
/// 检测到剪贴板新图后存为 PNG 到 save_dir。无论成败都恢复窗口。
#[tauri::command]
pub async fn quick_screenshot(
    app: tauri::AppHandle,
    save_dir: String,
) -> Result<ScreenshotResult, String> {
    let dir = resolve_screenshot_dir(&save_dir, &app);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建截图目录失败：{e}"))?;
    let dir_str = dir.to_string_lossy().into_owned();

    // 1. 隐藏本软件全部可见窗口（含悬浮条/面板/FPS 悬浮窗）
    let mut ctx = HideCtx { me: std::process::id(), hidden: Vec::new() };
    unsafe {
        let _ = EnumWindows(
            Some(hide_own),
            LPARAM(&mut ctx as *mut HideCtx as isize),
        );
    }
    std::thread::sleep(Duration::from_millis(350)); // 等桌面重绘，避免隐藏残影

    let result = (|| -> Result<ScreenshotResult, String> {
        // 2. 呼出系统截图框选；记录剪贴板序号以识别「新」图（不破坏原有剪贴板内容）
        let base_seq = unsafe { GetClipboardSequenceNumber() };
        send_combo(&[VK_LWIN, VK_SHIFT], VIRTUAL_KEY(0x53))?; // Win+Shift+S

        // 3. 等待框选完成：序号变化且出现图像格式；超时（30s）视为取消
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut detected = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(400));
            if unsafe { GetClipboardSequenceNumber() } == base_seq {
                continue;
            }
            // 8 = CF_DIB
            if unsafe { IsClipboardFormatAvailable(8) }.is_ok() {
                detected = true;
                break;
            }
        }
        if !detected {
            return Ok(ScreenshotResult { saved: false, path: None });
        }

        // 4. 剪贴板图像 → PNG（失败短暂重试，防剪贴板被瞬时占用）
        let mut last_err = String::new();
        for _ in 0..6 {
            match save_clipboard_image(&dir_str) {
                Ok(path) => return Ok(ScreenshotResult { saved: true, path: Some(path) }),
                Err(e) => {
                    last_err = e;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        Err(last_err)
    })();

    restore_windows(&ctx.hidden);
    result
}

/// 打开截图保存目录（不存在则先创建），返回实际目录供 UI 展示
#[tauri::command]
pub async fn quick_open_screenshot_dir(
    app: tauri::AppHandle,
    save_dir: String,
) -> Result<String, String> {
    let dir = resolve_screenshot_dir(&save_dir, &app);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
    Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("打开资源管理器失败：{e}"))?;
    Ok(dir.to_string_lossy().into_owned())
}

/// 返回主页：显示桌面（再按一次可还原窗口）
#[tauri::command]
pub async fn quick_show_desktop() -> Result<(), String> {
    send_combo(&[VK_LWIN], VIRTUAL_KEY(0x44)) // 'D'
}

/// 关机：立即执行 Windows 关机
#[tauri::command]
pub async fn quick_shutdown() -> Result<(), String> {
    crate::spawn_no_window("shutdown")
        .args(["/s", "/t", "0"])
        .spawn()
        .map_err(|e| format!("关机命令启动失败：{e}"))?;
    Ok(())
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

fn window_title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..(n.max(0) as usize).min(buf.len())])
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    hr.is_ok() && cloaked != 0
}

struct EnumCtx {
    me: u32,
    found: Option<HWND>,
}

/// Z 序自顶向下找第一个「非本应用、可见、非 cloaked、有标题」的普通窗口
unsafe extern "system" fn find_foreign(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if !IsWindowVisible(hwnd).as_bool() || window_pid(hwnd) == ctx.me || is_cloaked(hwnd) {
        return BOOL(1);
    }
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if (ex_style as u32 & WS_EX_TOOLWINDOW.0) != 0 {
        return BOOL(1);
    }
    let class = window_class(hwnd);
    // 桌面与任务栏不属于可关闭的「前台应用」
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
        return BOOL(1);
    }
    if window_title(hwnd).is_empty() {
        return BOOL(1);
    }
    ctx.found = Some(hwnd);
    BOOL(0) // 找到即停
}

fn topmost_foreign_window(me: u32) -> Option<HWND> {
    let mut ctx = EnumCtx { me, found: None };
    unsafe {
        let _ = EnumWindows(
            Some(find_foreign),
            LPARAM(&mut ctx as *mut EnumCtx as isize),
        );
    }
    ctx.found
}

/// 干跑：返回「强制关闭游戏」实际会命中的窗口（前台 / 兜底 Z 序），供 examples 验证
pub fn close_game_target() -> Option<(u32, String)> {
    let me = std::process::id();
    let mut target = unsafe { GetForegroundWindow() };
    if target.is_invalid() || window_pid(target) == me {
        target = topmost_foreign_window(me)?;
    }
    let title = window_title(target);
    let shown = if title.is_empty() { window_class(target) } else { title };
    Some((window_pid(target), shown))
}

/// 干跑（Z 序兜底路径）：跳过前台判断，直接返回 Z 序最高的可关闭普通窗口
pub fn close_game_target_fallback() -> Option<(u32, String)> {
    let hwnd = topmost_foreign_window(std::process::id())?;
    let title = window_title(hwnd);
    let shown = if title.is_empty() { window_class(hwnd) } else { title };
    Some((window_pid(hwnd), shown))
}

/// 强制关闭游戏：向前台窗口发送 Alt+F4 等效的关闭消息，返回目标窗口标题
#[tauri::command]
pub async fn quick_close_game() -> Result<String, String> {
    let me = std::process::id();
    let mut target = unsafe { GetForegroundWindow() };
    if target.is_invalid() || window_pid(target) == me {
        target = topmost_foreign_window(me).ok_or("未找到可关闭的前台窗口")?;
    }
    let title = window_title(target);
    // Alt+F4 → 系统向焦点窗口发 WM_SYSCOMMAND(SC_CLOSE)，由应用自行走关闭流程
    let sent = unsafe {
        PostMessageW(Some(target), WM_SYSCOMMAND, WPARAM(SC_CLOSE as usize), LPARAM(0))
    };
    if sent.is_err() {
        let fallback =
            unsafe { PostMessageW(Some(target), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if fallback.is_err() {
            return Err("关闭消息发送失败".into());
        }
    }
    let shown = if title.is_empty() { window_class(target) } else { title };
    Ok(if shown.is_empty() { "前台窗口".into() } else { shown })
}
