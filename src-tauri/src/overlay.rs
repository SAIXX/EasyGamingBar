//! 游戏内覆盖显示：让悬浮条像 Xbox Game Bar 一样浮在游戏画面上，且不跟游戏抢键盘。
//!
//! 三件事：
//! 1. `WS_EX_NOACTIVATE` —— 显示时不夺取系统焦点，键盘输入仍然归游戏；
//!    用户鼠标真正点进来时才由前端调 focus_overlay 取焦点（键鼠/手柄导航需要焦点）。
//! 2. 每次显示都重新 SetWindowPos(HWND_TOPMOST) —— 游戏切分辨率 / 重建窗口后 z 序会被
//!    重排，光靠创建时的 alwaysOnTop 不够。
//! 3. 取焦点走 AttachThreadInput 兜底 —— 前台不是本进程时 SetForegroundWindow 通常只闪任务栏。
//!
//! 硬边界：真正的「独占全屏（classic FSE）」下系统不再合成桌面，任何第三方窗口都画不出来
//! （Xbox Game Bar 自己也不行，表现为屏幕闪两下）。能浮起来的前提是 Windows 的
//! Fullscreen Optimizations 把游戏变成 DWM 合成的无边框全屏，或游戏本身用无边框全屏。

use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};
use std::sync::Once;
use std::time::Duration;
use tauri::{AppHandle, Manager, WebviewWindow};
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    GetGUIThreadInfo, GetForegroundWindow, GetShellWindow, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, GUITHREADINFO, IsChild, IsIconic, IsWindow,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, SwitchToThisWindow,
    GWL_EXSTYLE, GW_CHILD, HWND_TOPMOST, SW_RESTORE, SW_SHOW, SW_SHOWNA, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WINDOW_EX_STYLE, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
};


/// 内嵌的 FPSOverlay v1.8.0（GPLv3，github.com/aneeskhan47/fps-overlay）：
/// ETW 方式统计真实游戏内帧率（需管理员运行，反作弊安全）
const FPS_OVERLAY_ZIP: &[u8] = include_bytes!("../resources/fps-overlay.zip");

fn fps_overlay_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_default()
        .join("fps-overlay")
}

fn overlay_running() -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok();
    let Some(snap) = snap else { return false };
    unsafe {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile);
                if name.eq_ignore_ascii_case("overlay.exe") {
                    let _ = CloseHandle(snap);
                    return true;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    false
}

/// 首次启动：把内嵌的 FPSOverlay 解压到用户目录（config.ini 需写在可写位置）
fn ensure_overlay_extracted(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = fps_overlay_dir(app);
    let exe = dir.join("overlay.exe");
    if exe.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip_path = dir.join("_overlay.zip");
    std::fs::write(&zip_path, FPS_OVERLAY_ZIP).map_err(|e| e.to_string())?;
    let script = format!(
        "powershell -NoProfile -Command \"Expand-Archive -Path '{}' -DestinationPath '{}' -Force\"",
        zip_path.display(),
        dir.display()
    );
    let st = crate::spawn_no_window("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&zip_path);
    if !st.success() || !exe.exists() {
        return Err("FPSOverlay 解压失败".into());
    }
    Ok(dir)
}

/// 启动外部 FPSOverlay（管理员权限，首次弹 UAC）；已在运行则忽略
#[tauri::command]
pub fn fps_overlay_launch(app: tauri::AppHandle) -> Result<(), String> {
    if overlay_running() {
        return Ok(());
    }
    let dir = ensure_overlay_extracted(&app)?;
    let exe = dir.join("overlay.exe");
    if !exe.exists() {
        return Err("overlay.exe 缺失".into());
    }
    let exe_s = exe.display().to_string();
    let exe_w: Vec<u16> = exe_s.encode_utf16().chain(std::iter::once(0)).collect();
    let dir_w: Vec<u16> = dir.display().to_string().encode_utf16().chain(std::iter::once(0)).collect();
    let ret = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("runas"),
            windows::core::PCWSTR(exe_w.as_ptr()),
            None,
            windows::core::PCWSTR(dir_w.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    if ret.0 as i32 <= 32 {
        return Err(format!("启动失败（ShellExecute 代码 {}）", ret.0 as i32));
    }
    Ok(())
}

/// 停止外部 FPSOverlay
#[tauri::command]
pub fn fps_overlay_stop() -> Result<(), String> {
    crate::spawn_no_window("cmd")
        .args(["/C", "taskkill", "/F", "/IM", "overlay.exe"])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

type Win = WebviewWindow;

fn raw(win: &Win) -> Option<HWND> {
    win.hwnd().ok().map(|h| HWND(h.0))
}

/* ===================== 两种窗口模式 =====================
 * MODE 1 `GameOverlay`：游戏中显示视频/直播/悬浮信息。窗口带 WS_EX_NOACTIVATE，
 *   不抢游戏的键盘与鼠标焦点，游戏继续收键盘/手柄（画中画该有的行为）。
 * MODE 2 `GameBarInteractive`：用户用手柄快捷键（Xbox 键 / X 键）唤出菜单。
 *   此时必须**临时摘掉 WS_EX_NOACTIVATE**：带这个样式的窗口永远不会成为
 *   前台窗口，SetForegroundWindow 对它基本无效——「界面显示出来了但焦点还在
 *   游戏里」就是这么来的。退出时再把样式还原回去，不破坏 Overlay 语义。
 * 切换样式必须 SetWindowPos(SWP_FRAMECHANGED) 让系统重新读取扩展样式。 */

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GameBarMode {
    /// 覆盖层：不抢焦点
    GameOverlay,
    /// 交互菜单：必须成为前台窗口并接管手柄导航
    GameBarInteractive,
}

static GAMEBAR_MODE: AtomicU8 = AtomicU8::new(0); // 0=Overlay, 1=Interactive

pub fn gamebar_mode() -> GameBarMode {
    if GAMEBAR_MODE.load(Ordering::Relaxed) == 1 {
        GameBarMode::GameBarInteractive
    } else {
        GameBarMode::GameOverlay
    }
}

fn set_gamebar_mode(m: GameBarMode) {
    let v = if m == GameBarMode::GameBarInteractive { 1 } else { 0 };
    if GAMEBAR_MODE.swap(v, Ordering::Relaxed) != v {
        log::info!("[gamebar] 输入模式 → {:?}", m);
    }
}

/// 手柄导航是否被界面捕获（GameBarInteractive 且前台确实在我们这儿）
pub fn gamepad_capture() -> bool {
    gamebar_mode() == GameBarMode::GameBarInteractive && foreground_is_ours()
}

/// 读写扩展样式并让改动生效（GetWindowLongPtr → SetWindowLongPtr → SetWindowPos）
fn apply_ex_style(win: &Win, add: WINDOW_EX_STYLE, remove: WINDOW_EX_STYLE) -> u32 {
    let Some(h) = raw(win) else { return 0 };
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let want = (ex | add.0) & !remove.0;
        if want != ex {
            let _ = SetWindowLongPtrW(h, GWL_EXSTYLE, want as isize);
            // SWP_FRAMECHANGED：不重发这条消息，系统不会用新的扩展样式重画非客户区
            let _ = SetWindowPos(
                h,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        want
    }
}

/// 切到 / 退出「可激活」样式（只针对悬浮条；直播窗等纯覆盖层不动）
fn set_interactive_style(win: &Win, on: bool) -> u32 {
    if win.label() != "bar" {
        return 0;
    }
    if on {
        // 摘掉 NOACTIVATE；顺手确保不是点击穿透的（TRANSPARENT 会吃掉键鼠）
        apply_ex_style(
            win,
            WS_EX_TOPMOST,
            WINDOW_EX_STYLE(WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0),
        )
    } else {
        apply_ex_style(win, WINDOW_EX_STYLE(WS_EX_NOACTIVATE.0 | WS_EX_TOPMOST.0), WINDOW_EX_STYLE(0))
    }
}

/// 覆盖层样式：不激活 + 常置顶。每次置顶前都会先校验，窗口被重建也不会丢。
/// **交互模式下不套 NOACTIVATE**（否则永远取不到前台，见上面的模式说明）。
fn ensure_overlay_style(win: &Win) {
    let Some(h) = raw(win) else { return };
    if gamebar_mode() == GameBarMode::GameBarInteractive && win.label() == "bar" {
        set_interactive_style(win, true);
        return;
    }
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let want = WINDOW_EX_STYLE(ex as u32) | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
        if want.0 as isize != ex {
            let _ = SetWindowLongPtrW(h, GWL_EXSTYLE, want.0 as isize);
        }
    }
}

/* ---------------- 完整激活 / 退出（GAMEBAR_INTERACTIVE） ---------------- */

/// 取 WebView2 的焦点：先把焦点给宿主窗口，再给 WebView2 的子窗口。
/// 只让 HTML 元素 focus() 是不够的——原生窗口还停在游戏上时，
/// 键盘/手柄的原生路由根本到不了页面。
///
/// 两个坑：
/// - `SetFocus` 只对**调用线程自己**的窗口可靠：从轮询线程调时必须先
///   `AttachThreadInput` 挂到窗口所属线程的输入队列，否则静默无效；
/// - 校验别用 `GetFocus`（它只看调用线程的队列），要用全局的
///   `GetGUIThreadInfo().hwndFocus`，任何线程调都准。
fn focus_webview_child(h: HWND) -> bool {
    unsafe {
        let tid = GetWindowThreadProcessId(h, None);
        let cur = GetCurrentThreadId();
        let attached = if tid != 0 && tid != cur {
            AttachThreadInput(cur, tid, true).as_bool()
        } else {
            false
        };
        let mut child = GetWindow(h, GW_CHILD).unwrap_or(h);
        // WebView2 的宿主子窗口链：一层层往下取第一个子窗即可
        let mut guard = 0;
        while !child.is_invalid() && guard < 4 {
            match GetWindow(child, GW_CHILD) {
                Ok(deeper) if !deeper.is_invalid() => child = deeper,
                _ => break,
            }
            guard += 1;
        }
        if child.is_invalid() {
            child = h;
        }
        let _ = SetFocus(Some(child));
        if attached {
            let _ = AttachThreadInput(cur, tid, false);
        }
        // 全局校验：键盘焦点真的落在我们（或 WebView2 子窗）上了吗
        let mut gi = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut gi).is_ok() {
            let f = gi.hwndFocus;
            f == h || f == child || IsChild(h, f).as_bool()
        } else {
            false
        }
    }
}

/// 合成一次 Alt 按下-抬起，解除 SetForegroundWindow 的前台锁。
/// 手柄是被动轮询，按键不会像键盘热键那样让系统给我们前台许可；
/// 注入一次瞬时 Alt 让系统认为「用户正在输入」即可解锁——这是
/// Chrome 等前台工具的标准做法。单次注入（down+up 连发），不是暴力循环。
fn unlock_foreground_lock() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, VK_MENU,
    };
    unsafe {
        let mut inp = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        wScan: 0,
                        dwFlags: KEYEVENTF_EXTENDEDKEY,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        wScan: 0,
                        // 必须与按下时同为 EXTENDEDKEY（右 Alt），否则抬起的是左 Alt，
                        // 右 Alt 会永久卡在按下状态 —— 表现为全局「打字出别的字符」。
                        dwFlags: KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ];
        let n = SendInput(&inp, std::mem::size_of::<INPUT>() as i32);
        if n != 2 {
            log::warn!("[gamebar] 前台锁解锁注入未完全生效（{}）", n);
        }
    }
}

/// **完整激活流程**（手柄快捷键唤出菜单时用）：
/// 显示 → 切交互样式 → 置顶 → SetForegroundWindow → SetFocus → WebView2 焦点 → 逐项校验。
/// 不做暴力抢焦点：前台锁挡下时注入一次瞬时 Alt 解锁再试（共 2 轮），
/// 最后用一次 SwitchToThisWindow 兜底，之后如实报失败。
/// 返回是否真的拿到了前台窗口。
pub fn activate_gamebar(app: &AppHandle) -> bool {
    let Some(win) = app.get_webview_window("bar") else {
        log::warn!("[gamebar] 找不到悬浮条窗口，激活中止");
        return false;
    };
    let Some(h) = raw(&win) else {
        return false;
    };
    // ① 记下「把焦点让给我们的是谁」（多半是游戏）：退出时要原样还回去
    note_foreground();
    let prev_fg = unsafe { GetForegroundWindow() };

    // ② 确保真的显示出来（最小化时 SW_SHOW 不会还原）
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = ShowWindow(h, SW_SHOW);
    }
    // ③ 切到交互模式并摘掉 NOACTIVATE（这一步是能不能拿到前台的关键）
    set_gamebar_mode(GameBarMode::GameBarInteractive);
    let ex = set_interactive_style(&win, true);
    raise(&win);
    // ④ 正规激活（AttachThreadInput 兜底前台锁）
    let mut fg_ok = set_foreground_raw(h);
    let mut unlocked = false;
    if !fg_ok {
        // 前台锁：手柄输入不给前台许可 → 注入一次瞬时 Alt 解锁后重试
        unlock_foreground_lock();
        std::thread::sleep(Duration::from_millis(20));
        fg_ok = set_foreground_raw(h);
        unlocked = true;
    }
    if !fg_ok {
        // 最后一次兜底：SwitchToThisWindow（单次调用，非循环）
        unsafe { SwitchToThisWindow(h, true) };
        std::thread::sleep(Duration::from_millis(40));
        fg_ok = unsafe { GetForegroundWindow() == h };
    }
    // ⑤ 焦点：宿主窗口 → WebView2 子窗口
    let _ = unsafe { SetFocus(Some(h)) };
    let wv_ok = focus_webview_child(h);
    let cur_fg = unsafe { GetForegroundWindow() };

    log::info!(
        "[gamebar] activated\n\
         GameBar HWND: 0x{:X}\n\
         Previous Foreground HWND: 0x{:X}\n\
         Current Foreground HWND: 0x{:X}\n\
         GameBar Focus: {}\n\
         WebView2 Focus: {}\n\
         Input Mode: {:?}\n\
         Gamepad Capture: {}\n\
         ExStyle: 0x{:X}",
        h.0 as usize,
        prev_fg.0 as usize,
        cur_fg.0 as usize,
        if cur_fg == h { "YES" } else { "NO" },
        if wv_ok { "YES" } else { "NO" },
        gamebar_mode(),
        if gamepad_capture() { "YES" } else { "NO" },
        ex
    );
    log::info!(
        "[gamebar] 前台锁解锁注入: {}",
        if unlocked { "USED" } else { "UNUSED" }
    );
    if !fg_ok {
        log::warn!("[gamebar] 未能成为前台窗口（前台锁 / 游戏反抢）：手柄导航不会进界面");
    }
    fg_ok
}

/// 把任意覆盖层窗口强激活为前台窗口（前台锁兜底：瞬时 Alt 解锁 → 重试 →
/// SwitchToThisWindow 兜底），成功后再把键盘焦点推进 WebView2 子窗口。
/// 与 [`activate_gamebar`] 的激活段同源，但不动悬浮条样式/手柄模式。
///
/// **直播浏览器控制态必须用它**：WebView2 只把鼠标**按键**消息派发给活动窗口——
/// 实测（本机 dpr=2、3840×2160）窗口不在前台时，注入的 mousemove / wheel 能进页面，
/// mousedown/mouseup 进不去（被当成「激活点击」吞掉），表现为 A 键点了没反应、
/// 拖动失效。Tauri 的 `set_focus()` 就是裸 SetForegroundWindow，被前台锁挡下即静默失败。
pub fn force_foreground(win: &tauri::WebviewWindow) -> bool {
    let Some(h) = raw(win) else { return false };
    let mut ok = set_foreground_raw(h);
    if !ok {
        unlock_foreground_lock();
        std::thread::sleep(Duration::from_millis(20));
        ok = set_foreground_raw(h);
    }
    if !ok {
        unsafe { SwitchToThisWindow(h, true) };
        std::thread::sleep(Duration::from_millis(40));
        ok = unsafe { GetForegroundWindow() == h };
    }
    if ok {
        let _ = unsafe { SetFocus(Some(h)) };
        focus_webview_child(h);
    }
    ok
}

/// 该窗口当前是否就是系统前台窗口（控制态注入点击前的判定用，零副作用）
pub fn is_foreground(win: &tauri::WebviewWindow) -> bool {
    match raw(win) {
        Some(h) => unsafe { GetForegroundWindow() == h },
        None => false,
    }
}

/// 退出交互模式：还原覆盖层样式 → 放开手柄捕获 → 把前台还给游戏。
/// 必须在隐藏窗口**之前**调用：顺序反了 Windows 已经把前台塞给别人了。
pub fn deactivate_gamebar(app: &AppHandle, reason: &str) -> bool {
    if let Some(win) = app.get_webview_window("bar") {
        set_interactive_style(&win, false);
    }
    set_gamebar_mode(GameBarMode::GameOverlay);
    let ok = restore_foreground(reason);
    log::info!(
        "[gamebar] deactivated ({reason})\n\
         Input Mode: {:?}\n\
         Gamepad Capture: {}\n\
         Foreground Restored: {}",
        gamebar_mode(),
        if gamepad_capture() { "YES" } else { "NO" },
        if ok { "YES" } else { "NO" }
    );
    ok
}

/// 重新压到 topmost 层（不激活窗口，避免把游戏的键盘焦点抢走）
pub fn raise(win: &Win) {
    ensure_overlay_style(win);
    let Some(h) = raw(win) else { return };
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 兜底显示：Tauri 的 `show()` 对「最小化」或某些隐藏态的窗口不一定生效
/// （SW_SHOW 不会还原最小化窗口），这里直接走 Win32：先还原再显示。
/// 用 SW_SHOWNA（显示但不激活）——覆盖层不该抢走游戏的键盘焦点。
pub fn force_show(win: &Win) {
    let Some(h) = raw(win) else { return };
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = ShowWindow(h, SW_SHOWNA);
    }
    raise(win);
    // 仍不可见就退一步用会激活的 SW_SHOW（会短暂抢焦点，但总比「点了没反应」好）
    if !win.is_visible().unwrap_or(false) {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
        }
        raise(win);
        log::warn!("force_show：SW_SHOWNA 未生效，已回退 SW_SHOW");
    }
}

/// 窗口整个跑到屏幕外就拉回显示器顶部居中（不激活、不改尺寸）。
/// 场景：窗口最小化时 Windows 会把它挪到 (-32000,-32000)，前端若把这个坐标
/// 记下来再恢复，悬浮条就永远在屏幕外——界面「不见了」。这里是最后一道兜底。
pub fn ensure_onscreen(win: &Win) {
    let Some(h) = raw(win) else { return };
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(h, &mut r).is_err() {
            return;
        }
        let w = r.right - r.left;
        let hh = r.bottom - r.top;
        if w <= 0 || hh <= 0 {
            return;
        }
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTOPRIMARY);
        if mon.is_invalid() {
            return;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let m = mi.rcMonitor;
        let outside =
            r.right <= m.left || r.left >= m.right || r.bottom <= m.top || r.top >= m.bottom;
        if !outside {
            return;
        }
        let x = m.left + ((m.right - m.left) - w) / 2;
        let y = m.top + 8;
        let _ = SetWindowPos(h, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE);
        log::warn!(
            "悬浮条位置在屏幕外（{},{}）→ 已拉回显示器顶部（{},{}）",
            r.left,
            r.top,
            x,
            y
        );
    }
}

/// 把某个窗口提到前台（尽力而为）：前台是别的进程时先 AttachThreadInput，
/// 否则 SetForegroundWindow 常被前台锁挡下、只闪一下 taskbar 图标。
/// SetForegroundWindow 的返回值不可信（被前台锁挡下也可能报成功），所以每次
/// 调用后都校验前台真的切过来了；没切成再等 30ms 重试一次——游戏主线程繁忙时
/// 一次尝试经常落空。仍失败返回 false 并记日志（否则表现为「点了 UI 却打不了字」，
/// 无迹可查）。返回 false 通常意味着游戏正在抢回前台，调用方可自行决定是否重试。
pub fn focus_window(win: &Win) -> bool {
    let Some(h) = raw(win) else { return false };
    // 记下「把焦点让给我们的是谁」（多半是游戏）：收起界面时要原样还回去
    note_foreground();
    if set_foreground_raw(h) {
        return true;
    }
    log::warn!(
        "focus_window：两次尝试后前台仍未切到目标窗口（前台锁 / 游戏抢焦点），label={:?}",
        win.label()
    );
    false
}

/// 最近一次「非本应用」的前台窗口（通常就是游戏）。
/// 必须在收起界面时把前台还给它——本应用窗口一 hide，Windows 会顺手把前台交给
/// Z 序里的下一个窗口，表现就是「退出悬浮条后跳到桌面 / 别的窗口 / 任务视图」。
/// Xbox Game Bar 不这么跳，是因为它压根不拿走游戏的前台。
static RESTORE_TARGET: AtomicIsize = AtomicIsize::new(0);
static TRACKER_ONCE: Once = Once::new();

/// 窗口是否属于本进程（悬浮条 / 面板 / 设置中心 …）。
/// 用进程号判定，不必维护窗口清单：新窗口、前端自己建的窗都在同一进程里。
fn is_own_window(h: HWND) -> bool {
    if h.is_invalid() {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// 最近一次「非本应用」的前台窗口（通常就是游戏）。窗口可能已被销毁，调用方要校验。
pub fn last_foreign_foreground() -> Option<HWND> {
    let h = HWND(RESTORE_TARGET.load(Ordering::Relaxed) as *mut core::ffi::c_void);
    if h.is_invalid() || !unsafe { IsWindow(Some(h)).as_bool() } {
        None
    } else {
        Some(h)
    }
}

/// 窗口是否属于本应用进程（悬浮条 / 面板 / 设置中心 …）
pub fn own_window(h: HWND) -> bool {
    is_own_window(h)
}

/// 记录当前前台窗口，但只记「不是本应用」的那些（自己的窗口统统跳过）。
pub fn note_foreground() {
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() || fg == unsafe { GetShellWindow() } {
        return;
    }
    if is_own_window(fg) {
        return;
    }
    RESTORE_TARGET.store(fg.0 as isize, Ordering::Relaxed);
}

/// 低频轮询前台（400ms）兜底：焦点不一定只从 focus_window 这条路径被拿走
/// （force_show 的 SW_SHOW 回退、新窗口创建等都会偷偷激活自己），
/// 只在取焦点时记会漏记这些场景，导致「还回去」时没有目标。
pub fn start_foreground_tracker() {
    TRACKER_ONCE.call_once(|| {
        if std::thread::Builder::new()
            .name("fg-tracker".into())
            .spawn(|| loop {
                note_foreground();
                std::thread::sleep(Duration::from_millis(400));
            })
            .is_err()
        {
            log::warn!("前台跟踪线程启动失败");
        }
    });
}

/// SetForegroundWindow + AttachThreadInput 兜底（前台是别的进程时直接
/// SetForegroundWindow 常被前台锁挡下），两次尝试后返回是否真的切过去了。
fn set_foreground_raw(h: HWND) -> bool {
    for attempt in 0..2 {
        if attempt > 0 {
            // 前台切换是异步生效的，立即重试多半还是失败，稍等片刻
            std::thread::sleep(Duration::from_millis(30));
        }
        unsafe {
            let fg = GetForegroundWindow();
            if fg == h {
                return true;
            }
            let fg_tid = GetWindowThreadProcessId(fg, None);
            let tid = GetCurrentThreadId();
            let mut attached = false;
            if fg_tid != 0 && fg_tid != tid {
                attached = AttachThreadInput(fg_tid, tid, true).as_bool();
            }
            let _ = SetForegroundWindow(h);
            if attached {
                let _ = AttachThreadInput(fg_tid, tid, false);
            }
            if GetForegroundWindow() == h {
                return true;
            }
        }
    }
    false
}

/// 收起界面前调用：把前台还给最近一次的外来窗口（一般是游戏）。
/// 前置条件：当前前台确实停在本应用窗口上——玩家中途 Alt+Tab 走了就不能把他拽回来。
/// 必须在隐藏本应用窗口「之前」调用：顺序反了 Windows 已经把前台塞给了别人。
pub fn restore_foreground(reason: &str) -> bool {
    let target = HWND(RESTORE_TARGET.load(Ordering::Relaxed) as *mut core::ffi::c_void);
    if target.is_invalid() {
        return false;
    }
    unsafe {
        if !is_own_window(GetForegroundWindow()) {
            // 焦点已经不在我们这儿（玩家自己切走了），别抢回来
            return false;
        }
        if !IsWindow(Some(target)).as_bool() {
            RESTORE_TARGET.store(0, Ordering::Relaxed);
            log::info!("还原前台（{reason}）：目标窗口已销毁，跳过");
            return false;
        }
        // 独占全屏的游戏丢了焦点会把自己最小化（D3D 设备丢失），
        // 不还原就 SetForegroundWindow，玩家就被留在桌面上了
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
            std::thread::sleep(Duration::from_millis(60));
        }
        let mut ok = set_foreground_raw(target);
        if !ok {
            // 前台锁：点击我们窗口的可能是页面内派发的合成点击（手柄 A），
            // 系统不认作"用户输入"，SetForegroundWindow 照样被挡 → 注入一次
            // 瞬时 Alt 解锁后重试（与 activate_gamebar 的激活段同款做法）。
            unlock_foreground_lock();
            std::thread::sleep(Duration::from_millis(20));
            ok = set_foreground_raw(target);
        }
        if !ok {
            unsafe { SwitchToThisWindow(target, true) };
            std::thread::sleep(Duration::from_millis(40));
            ok = unsafe { GetForegroundWindow() == target };
        }
        log::info!("还原前台（{reason}）到原窗口：ok={ok}");
        ok
    }
}

/// 当前前台窗口是不是本应用自己的（悬浮条 / 面板 / 设置中心 …）。
/// 用进程号判定，新窗口、前端自开的窗都算。
pub fn foreground_is_ours() -> bool {
    is_own_window(unsafe { GetForegroundWindow() })
}

/// 界面可见性（悬浮条为准）：可见且未最小化。Win+D 会把窗口最小化，
/// 而最小化窗口的 IsWindowVisible 仍为 true，所以两个条件都要看。
pub fn ui_awake(app: &AppHandle) -> bool {
    app.get_webview_window("bar")
        .map(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false))
        .unwrap_or(false)
}

/// 「强焦点」：把焦点死死按到悬浮条上（Xbox 键唤醒 / 手柄 X 键接管用）。
/// 游戏（尤其独占全屏）会在几帧内把前台抢回去，单次 SetForegroundWindow
/// 经常白做——这里先确保窗口真的显示出来，再在 ~500ms 内重试多次并逐次校验。
/// 返回最终是否真的拿到前台。
pub fn focus_ui_strong(app: &AppHandle) -> bool {
    let Some(bar) = app.get_webview_window("bar") else {
        return false;
    };
    // 先保证窗口真在屏幕上：最小化 / 被别的 topmost 盖住时先还原再提顶
    force_show(&bar);
    raise(&bar);
    for i in 0..6 {
        if focus_window(&bar) {
            return true;
        }
        if i % 2 == 1 {
            // 连续失败两次再补一次「显示+提顶」：有些游戏会把我们的窗口压下去
            force_show(&bar);
            raise(&bar);
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    log::warn!("focus_ui_strong：6 次尝试后仍未把前台切到悬浮条");
    false
}

/// 前台是否是一个铺满整块屏幕的窗口（用来判断「现在是不是在游戏里」）。
/// 检测的是「全屏」而不是「游戏」——全屏看视频同理，此时都不该抢焦点。
pub fn foreground_is_fullscreen_app() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg == GetShellWindow() {
            return false;
        }
        let monitor = MonitorFromWindow(fg, MONITOR_DEFAULTTONULL);
        if monitor.is_invalid() {
            return false;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return false;
        }
        let mut r: RECT = std::mem::zeroed();
        let _ = GetWindowRect(fg, &mut r);
        if r.right <= r.left || r.bottom <= r.top {
            return false;
        }
        r.left <= mi.rcMonitor.left
            && r.top <= mi.rcMonitor.top
            && r.right >= mi.rcMonitor.right
            && r.bottom >= mi.rcMonitor.bottom
    }
}

/// 点击穿透（锁直播画中画用）：只设 WS_EX_TRANSPARENT，绝不加 WS_EX_LAYERED——
/// wry 的 set_ignore_cursor_events 会带上 LAYERED，全屏游戏独立翻转时
/// 分层窗口会被踢出合成（画面消失只剩声音）。TRANSPARENT 只影响命中测试。
#[tauri::command]
pub fn set_click_through(app: AppHandle, label: String, on: bool) -> Result<(), String> {
    let Some(win) = app.get_webview_window(&label) else {
        return Err(format!("窗口不存在：{label}"));
    };
    let Some(h) = raw(&win) else {
        return Err("无法获取窗口句柄".into());
    };
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let want = if on {
            (ex | WS_EX_TRANSPARENT.0) & !WS_EX_LAYERED.0
        } else {
            ex & !(WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0)
        };
        if want != ex {
            SetWindowLongPtrW(h, GWL_EXSTYLE, want as isize);
        }
    }
    Ok(())
}

/// 前端自行隐藏窗口（关闭组件面板 / 设置中心等）前调用：这些窗口不走 Rust 的
/// 收起流程，但同样可能持有前台，hide 之后一样会把玩家甩到别的窗口。
#[tauri::command]
pub fn restore_focus() -> bool {
    restore_foreground("窗口自行隐藏")
}

/// 前端在鼠标真正点进来时调用：此时才取焦点（默认覆盖态不抢焦点）。
/// 返回是否拿到前台：false = 被前台锁挡下或游戏抢回，键盘暂时还在游戏那边。
#[tauri::command]
pub fn focus_overlay(app: AppHandle, label: Option<String>) -> bool {
    let label = label.unwrap_or_else(|| "bar".into());
    app.get_webview_window(&label)
        .is_some_and(|win| focus_window(&win))
}
