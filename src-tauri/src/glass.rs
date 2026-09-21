//! Windows 毛玻璃（Acrylic / Mica）+ 窗口圆角
//!
//! 前端在各窗口 onMounted 时调用 `set_glass`，成功后在 `<html>` 上标记 `glass`
//! 类，CSS 变量随即从纯色切到半透明（模糊由系统合成器提供，tint 只轻轻压暗，
//! 深浅交给前端变量调）。
//!
//! Acrylic 是真·背后内容模糊（毛玻璃感强），拖动时背景有短暂"拖影"；Mica 只
//! 染壁纸色、偏灰哑。统一 Acrylic 优先、Mica 降级，都不可用（Win10 1809 之前）
//! 返回 Err，前端维持纯色底。
//!
//! 另：毛玻璃背景板按窗口矩形铺满，UI 的 CSS 圆角外会露出矩形的直角边（用户
//! 反馈"圆角周围有直角/两侧不对称"），用 DWMWCP_ROUND 把窗口矩形圆角化对齐，
//! CSS 圆角同步取 8px（Win11 系统圆角半径）。

use tauri::Manager;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use window_vibrancy::{apply_acrylic, apply_mica, clear_acrylic, clear_mica};

type Win = tauri::WebviewWindow<tauri::Wry>;

/// 轻压暗的 acrylic tint（带蓝调的深色，避免灰哑）；再调就改前端 CSS 变量
const ACRYLIC_TINT: (u8, u8, u8, u8) = (10, 12, 20, 90);

/// 给窗口套毛玻璃，返回实际生效的模式；全部失败返回 None（前端维持纯色底）
pub fn apply(win: &Win, prefer_mica: bool) -> Option<&'static str> {
    let order: [&str; 2] = if prefer_mica {
        ["mica", "acrylic"]
    } else {
        ["acrylic", "mica"]
    };
    for mode in order {
        let res = match mode {
            "mica" => apply_mica(win, Some(true)),
            _ => apply_acrylic(win, Some(ACRYLIC_TINT)),
        };
        match res {
            Ok(()) => return Some(mode),
            Err(e) => log::debug!("毛玻璃 {mode} 应用失败：{e}"),
        }
    }
    None
}

/// 撤掉毛玻璃（悬浮条折叠时内容已移出窗口，背景板会露出一条黑带）
fn clear(win: &Win) {
    let _ = clear_acrylic(win);
    let _ = clear_mica(win);
}

/// DWM 窗口圆角化，让系统背景板与 UI 的 CSS 圆角（8px）对齐；
/// 撤毛玻璃时要还原为 DONOTROUND——否则透明窗口上会残留一圈系统边框圆角，
/// 看起来就是一个"空框"挂在屏幕上。
fn set_round(win: &Win, on: bool) {
    let Ok(hwnd) = win.hwnd() else {
        return;
    };
    let pref = DWM_WINDOW_CORNER_PREFERENCE(if on {
        DWMWCP_ROUND.0
    } else {
        windows::Win32::Graphics::Dwm::DWMWCP_DONOTROUND.0
    });
    unsafe {
        let _ = DwmSetWindowAttribute(
            HWND(hwnd.0),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

/// 独立内容窗口（如直播浏览器）四角圆角化：DWMWCP_ROUND（Win11 系统圆角半径，
/// 任意网页内容也生效——DWM 层裁剪，不依赖页面自身透明）
pub(crate) fn round_corners(win: &tauri::WebviewWindow, on: bool) {
    set_round(win, on);
}

/// 前端为「当前窗口」开/关毛玻璃；enabled=false 用于悬浮条折叠时撤底色
#[tauri::command]
pub fn set_glass(
    app: tauri::AppHandle,
    label: String,
    enabled: Option<bool>,
    prefer_mica: Option<bool>,
) -> Result<String, String> {
    let Some(win) = app.get_webview_window(&label) else {
        return Err(format!("窗口不存在：{label}"));
    };
    if enabled.unwrap_or(true) {
        // Mica 优先：Win11 22H2+ 上 Acrylic 与 WebView2 透明合成冲突会渲染成纯白，
        // Mica 走系统背景板（DWMWA_SYSTEMBACKDROP_TYPE）稳定；老系统降级 Acrylic
        match apply(&win, prefer_mica.unwrap_or(true)) {
            Some(mode) => {
                set_round(&win, true);
                Ok(mode.to_string())
            }
            None => Err("系统不支持毛玻璃".into()),
        }
    } else {
        clear(&win);
        set_round(&win, false);
        Ok("cleared".into())
    }
}
