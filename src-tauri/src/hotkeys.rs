//! 系统级全局快捷键（Win+Shift+字母）：
//! RegisterHotKey 绑定到专用线程的消息循环，触发后在子线程执行动作
//! 并向前端广播事件，设置面板据此刷新状态。
//! - Win+Shift+M  麦克风开/关
//! - Win+Shift+W  Wi-Fi 开/关
//! - Win+Shift+B  蓝牙开/关
//! - Win+Shift+A  飞行模式
//! - Win+Shift+L  直播画中画 锁定/解锁（锁定 = 鼠标点击穿透不干扰游戏）
//! - Win+Shift+K  显示 Windows 虚拟键盘（可在设置中心自定义）
//! - 显示/隐藏界面（默认 Win+Shift+G）：可在设置中心自定义，占用时提示
use crate::overlay;
use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{TryRecvError, SyncSender};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, DispatchMessageW, GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY,
};

const MODS: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(MOD_WIN.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0);

// 动作 id 同时是 RegisterHotKey 的 key id
const ID_MIC: i32 = 1;
const ID_WIFI: i32 = 2;
const ID_BT: i32 = 3;
const ID_AIRPLANE: i32 = 4;
const ID_LIVE_LOCK: i32 = 5;
const ID_TOGGLE_UI: i32 = 6;
const ID_FOCUS_UI: i32 = 8;
const ID_KEYBOARD: i32 = 7;

/// 线程消息：唤醒消息循环去处理重注册请求
const WM_RELOAD_HOTKEY: u32 = 0x8001; // WM_APP + 1

#[derive(Clone, Serialize)]
pub struct Binding {
    pub action: String,
    pub key: String,
    pub registered: bool,
}

static BINDINGS: Mutex<Vec<Binding>> = Mutex::new(Vec::new());
/// 快捷键线程 id（0 = 未启动），用于 PostThreadMessageW 唤醒
static THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// 重注册请求队列：(action, mods, vk, 结果回传)
type ReloadReq = (String, u32, u32, std::sync::mpsc::Sender<bool>);
static RELOAD_TX: Mutex<Option<SyncSender<ReloadReq>>> = Mutex::new(None);

#[tauri::command]
pub fn get_hotkey_bindings() -> Vec<Binding> {
    BINDINGS.lock().map(|b| b.clone()).unwrap_or_default()
}

/// 解析 "Win+Shift+G" 形式的组合键；必须含 Win / Ctrl / Alt 至少一个修饰键
fn parse_combo(s: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let mut mods = MOD_NOREPEAT.0;
    let mut vk: Option<u32> = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "win" => mods |= MOD_WIN.0,
            "ctrl" | "control" => mods |= MOD_CONTROL.0,
            "alt" => mods |= MOD_ALT.0,
            "shift" => mods |= MOD_SHIFT.0,
            k => {
                if vk.is_some() {
                    return Err(format!("无效的快捷键：{s}"));
                }
                vk = Some(key_vk(k).ok_or_else(|| format!("不支持的按键：{part}"))?);
            }
        }
    }
    let vk = vk.ok_or_else(|| format!("无效的快捷键：{s}"))?;
    if mods & (MOD_WIN.0 | MOD_CONTROL.0 | MOD_ALT.0) == 0 {
        return Err("组合键必须包含 Win / Ctrl / Alt 至少一个修饰键".into());
    }
    Ok((HOT_KEY_MODIFIERS(mods), vk))
}

/// 按键名 → 虚拟键码（字母 / 数字 / F1-F24 / Space 等名称键）
fn key_vk(k: &str) -> Option<u32> {
    let b = k.as_bytes();
    if b.len() == 1 {
        let c = b[0].to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
        return None;
    }
    match k.to_ascii_lowercase().as_str() {
        "space" | "spacebar" => return Some(0x20),
        "tab" => return Some(0x09),
        "enter" | "return" => return Some(0x0D),
        "up" => return Some(0x26),
        "down" => return Some(0x28),
        "left" => return Some(0x25),
        "right" => return Some(0x27),
        _ => {}
    }
    if let Some(n) = k
        .strip_prefix('F')
        .or_else(|| k.strip_prefix('f'))
        .and_then(|r| r.parse::<u32>().ok())
    {
        if (1..=24).contains(&n) {
            return Some(0x6F + n); // VK_F1 = 0x70
        }
    }
    None
}

/// 设置自定义唤醒键：解析 → 交给快捷键线程重注册 → 返回占用结果
#[tauri::command]
pub fn set_ui_hotkey(key: String) -> Result<(), String> {
    set_action_hotkey("toggle-ui".into(), key)
}

/// 设置任意可重绑的快捷键（如 "keyboard"），返回占用结果
#[tauri::command]
pub fn set_action_hotkey(action: String, key: String) -> Result<(), String> {
    let (mods, vk) = parse_combo(&key)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    {
        let guard = RELOAD_TX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(tx) = guard.as_ref() else {
            return Err("快捷键线程未就绪".into());
        };
        tx.send((action, mods.0, vk, reply_tx))
            .map_err(|_| "快捷键线程已退出".to_string())?;
    }
    let tid = THREAD_ID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_RELOAD_HOTKEY, WPARAM::default(), LPARAM::default());
        }
    }
    match reply_rx.recv_timeout(Duration::from_millis(1500)) {
        Ok(true) => Ok(()),
        Ok(false) => Err("该快捷键已被其他程序占用，请换一个组合".into()),
        Err(_) => Err("设置超时，请重试".into()),
    }
}

/// 动作名 → 可重绑的快捷键 id（仅 toggle-ui / keyboard 支持运行期重绑）
fn action_id(action: &str) -> Option<i32> {
    match action {
        "toggle-ui" => Some(ID_TOGGLE_UI),
        "focus-ui" => Some(ID_FOCUS_UI),
        "keyboard" => Some(ID_KEYBOARD),
        _ => None,
    }
}

fn register(ids: &[(i32, &str, HOT_KEY_MODIFIERS, u32, String)]) -> Vec<Binding> {
    let mut out = Vec::new();
    for &(id, action, mods, vk, ref label) in ids {
        // MOD_NOREPEAT：RegisterHotKey 默认在按住组合键时**连续重复触发** WM_HOTKEY
        // （每秒十几次）。不加这个标志，用户按住唤醒键就会一秒触发十几次 toggle，
        // 而每次 toggle 又各自 spawn 子线程并发改窗口可见性 → 状态互相打架，
        // 表现为「呼出后关不掉」（日志里一秒内连续多条「已隐藏」就是这个）。
        let mods = HOT_KEY_MODIFIERS(mods.0 | MOD_NOREPEAT.0);
        let ok = unsafe { RegisterHotKey(None, id, mods, vk) }.is_ok();
        if !ok {
            log::warn!("快捷键 {label} 注册失败（可能被其他程序占用）");
        }
        out.push(Binding {
            action: action.into(),
            key: label.clone(),
            registered: ok,
        });
    }
    out
}

/// 在 setup 阶段调用；启动常驻线程。
/// 显示/隐藏界面 的组合键从配置读取（settings.uiHotkey，缺省 Win+Shift+G），
/// 运行期可经 set_ui_hotkey 在本线程内重注册。
pub fn init(app: AppHandle) {
    // 初始组合键：配置 > 默认；解析失败回退默认
    let ui_key = tauri::async_runtime::block_on(async {
        crate::commands::load_config(app.clone())
            .await
            .ok()
            .and_then(|v| {
                v["settings"]["uiHotkey"]
                    .as_str()
                    .map(str::to_string)
            })
    })
    .unwrap_or_else(|| "Win+Shift+G".into());
    let (ui_mods, ui_vk) = parse_combo(&ui_key).unwrap_or_else(|e| {
        log::warn!("配置的唤醒键 {ui_key} 无效（{e}），回退 Win+Shift+G");
        parse_combo("Win+Shift+G").expect("默认键必可解析")
    });
    let ui_label = parse_combo(&ui_key)
        .map(|_| ui_key.clone())
        .unwrap_or_else(|_| "Win+Shift+G".into());

    // 显示虚拟键盘组合键：配置 > 默认 Win+Shift+K
    let kb_key = tauri::async_runtime::block_on(async {
        crate::commands::load_config(app.clone())
            .await
            .ok()
            .and_then(|v| v["settings"]["keyboardHotkey"].as_str().map(str::to_string))
    })
    .unwrap_or_else(|| "Win+Shift+K".into());
    let (kb_mods, kb_vk) = parse_combo(&kb_key).unwrap_or_else(|e| {
        log::warn!("配置的键盘键 {kb_key} 无效（{e}），回退 Win+Shift+K");
        parse_combo("Win+Shift+K").expect("默认键必可解析")
    });
    let kb_label = parse_combo(&kb_key)
        .map(|_| kb_key.clone())
        .unwrap_or_else(|_| "Win+Shift+K".into());

    // 呼出鼠标（抢焦点）组合键：配置 > 默认 Ctrl+Alt+F
    let focus_key = tauri::async_runtime::block_on(async {
        crate::commands::load_config(app.clone())
            .await
            .ok()
            .and_then(|v| v["settings"]["focusHotkey"].as_str().map(str::to_string))
    })
    .unwrap_or_else(|| "Ctrl+Alt+F".into());
    let (focus_mods, focus_vk) = parse_combo(&focus_key).unwrap_or_else(|e| {
        log::warn!("配置的呼出鼠标键 {focus_key} 无效（{e}），回退 Ctrl+Alt+F");
        parse_combo("Ctrl+Alt+F").expect("默认键必可解析")
    });
    let focus_label = parse_combo(&focus_key)
        .map(|_| focus_key.clone())
        .unwrap_or_else(|_| "Ctrl+Alt+F".into());

    std::thread::spawn(move || {
        THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        let (reload_tx, reload_rx) = std::sync::mpsc::sync_channel::<ReloadReq>(8);
        if let Ok(mut slot) = RELOAD_TX.lock() {
            *slot = Some(reload_tx);
        }

        let ids = [
            (ID_MIC, "mic", MODS, 0x4Du32, "Win+Shift+M".to_string()),
            (ID_WIFI, "wifi", MODS, 0x57u32, "Win+Shift+W".to_string()),
            (ID_BT, "bluetooth", MODS, 0x42u32, "Win+Shift+B".to_string()),
            (ID_AIRPLANE, "airplane", MODS, 0x41u32, "Win+Shift+A".to_string()),
            (
                ID_LIVE_LOCK,
                "live-lock",
                MODS,
                0x4Cu32,
                "Win+Shift+L".to_string(),
            ),
            (ID_TOGGLE_UI, "toggle-ui", ui_mods, ui_vk, ui_label),
            (
                ID_FOCUS_UI,
                "focus-ui",
                focus_mods,
                focus_vk,
                focus_label,
            ),
            (ID_KEYBOARD, "keyboard", kb_mods, kb_vk, kb_label),
        ];
        let bindings = register(&ids);
        if let Ok(mut slot) = BINDINGS.lock() {
            *slot = bindings;
        }

        let mut msg = MSG::default();
        loop {
            // 处理重注册请求（由 WM_RELOAD_HOTKEY 唤醒后到达此处）
            loop {
                match reload_rx.try_recv() {
                    Ok((action, mods, vk, reply)) => {
                        if let Some(id) = action_id(&action) {
                            unsafe {
                                let _ = UnregisterHotKey(None, id);
                            }
                            let ok = unsafe {
                                RegisterHotKey(
                                    None,
                                    id,
                                    HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0),
                                    vk,
                                )
                            }
                            .is_ok();
                            if ok {
                                let label = format_combo(HOT_KEY_MODIFIERS(mods), vk);
                                if let Ok(mut slot) = BINDINGS.lock() {
                                    for b in slot.iter_mut() {
                                        if b.action == action {
                                            b.key = label.clone();
                                            b.registered = true;
                                        }
                                    }
                                }
                                log::info!("快捷键 {action} 已改为 {label}");
                            } else {
                                log::warn!("新快捷键注册失败（被占用），保留原组合");
                            }
                            let _ = reply.send(ok);
                        } else {
                            let _ = reply.send(false);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }

            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if ret.0 <= 0 {
                break;
            }
            if msg.message == WM_HOTKEY {
                let app = app.clone();
                let id = msg.wParam.0 as i32;
                // 无线电切换可能阻塞数秒，放子线程执行，消息循环继续响应
                std::thread::spawn(move || run_action(&app, id));
            }
            unsafe {
                DispatchMessageW(&msg);
            }
        }
    });
}

/// 组合键 → 展示文案（Win+Ctrl+Alt+Shift+键名，与设置中心写入格式一致）
fn format_combo(mods: HOT_KEY_MODIFIERS, vk: u32) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods.0 & MOD_WIN.0 != 0 {
        parts.push("Win");
    }
    if mods.0 & MOD_CONTROL.0 != 0 {
        parts.push("Ctrl");
    }
    if mods.0 & MOD_ALT.0 != 0 {
        parts.push("Alt");
    }
    if mods.0 & MOD_SHIFT.0 != 0 {
        parts.push("Shift");
    }
    let key = match vk {
        0x20 => "Space".to_string(),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).unwrap_or('?').to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("0x{vk:02X}"),
    };
    parts.push(&key);
    parts.join("+")
}

/// 执行快捷键动作并广播结果事件
fn run_action(app: &AppHandle, id: i32) {
    let result: Result<(&str, serde_json::Value), String> = match id {
        ID_MIC => crate::audio::toggle_default_input_mute()
            .map(|muted| ("sys://mic", serde_json::json!({ "muted": muted }))),
        ID_WIFI => tauri::async_runtime::block_on(async {
            let s = crate::net::get_radio_status().await?;
            let on = crate::net::set_wifi_enabled(!s.wifi_on).await?;
            Ok(("sys://radios", serde_json::json!({ "wifi": on })))
        }),
        ID_BT => tauri::async_runtime::block_on(async {
            let s = crate::net::get_radio_status().await?;
            let on = crate::net::set_bt_enabled(!s.bt_on).await?;
            Ok(("sys://radios", serde_json::json!({ "bluetooth": on })))
        }),
        ID_AIRPLANE => tauri::async_runtime::block_on(async {
            let s = crate::net::get_radio_status().await?;
            let on = crate::net::set_airplane_mode(!s.airplane).await?;
            Ok(("sys://radios", serde_json::json!({ "airplane": on })))
        }),
        // 直播锁定切换：事件广播，直播工具条窗口负责实际执行
        ID_LIVE_LOCK => Ok(("live://toggle-lock", serde_json::Value::Null)),
        ID_TOGGLE_UI => {
            // 唤醒键按 settings.summonFocus 决定是否「呼出即接管」（默认开——用户要求
            // 全屏游戏呼出时焦点就在 bar 上）：true → force_focus 路径，全屏下走
            // activate_gamebar 强接管（摘 NOACTIVATE → ALT 解前台锁 → WebView 取焦点）。
            // 关掉则回到「与 Xbox Game Bar 同款」的不抢焦点行为（键鼠留在游戏，
            // 点进来 / 呼出鼠标键 / 手柄 X 键才接管）——真·独占全屏游戏不丢前台，
            // 也就不会闪一下（这个代价写进了设置项的描述里）。
            let grab = summon_focus(app);
            toggle_ui_visibility_with_focus(app, grab);
            Ok(("", serde_json::Value::Null))
        }
        ID_FOCUS_UI => {
            // 抓取焦点（呼出鼠标键）：显示 UI（如果隐藏）→ 强制把系统焦点给悬浮条。
            // 用户在游戏中按这个键 = "我要操作 UI"，焦点必须抢过来
            let bar = app.get_webview_window("bar");
            let visible = bar.as_ref().map(|w| w.is_visible().unwrap_or(false)).unwrap_or(false);
            if !visible {
                toggle_ui_visibility_with_focus(app, true); // 先显示（含强制焦点）
            } else {
                // 焦点切换前等修饰键松开（Ctrl+Alt+F 的 keydown 已在前台窗口里）
                wait_modifiers_released(Duration::from_millis(400));
                // 折叠态先展开（bar://reveal 的监听器自己判断，未折叠时是空操作）
                let _ = app.emit("bar://reveal", ());
                // 全屏游戏下前台锁顽固，走完整激活管线（摘 NOACTIVATE → ALT 解锁 →
                // SwitchToThisWindow 兜底 → WebView 子窗焦点）；bar 还停在覆盖态时
                // set_focus/focus_window 会被静默拒绝——「看得见控不了」的老根因
                overlay::activate_gamebar(app);
            }
            // 释放被游戏 ClipCursor 圈住的鼠标：呼出的光标要能划到 UI 上，
            // 否则光标只能在游戏窗口矩形内打转（全屏游戏常见）
            unsafe {
                let _ = ClipCursor(None);
            }
            // 手柄接管窗口也刷新
            crate::gamepad::note_wake();
            Ok(("", serde_json::Value::Null))
        }
        ID_KEYBOARD => {
            let _ = crate::commands::open_virtual_keyboard();
            Ok(("", serde_json::Value::Null))
        }
        _ => Ok(("", serde_json::Value::Null)),
    };
    match result {
        Ok((event, payload)) if !event.is_empty() => {
            let _ = app.emit(event, payload);
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("快捷键动作执行失败：{e}");
        }
    }
}

/// 显示/隐藏软件全部界面（唤醒键与托盘左键共用）。
/// 以悬浮条为锚做确定性切换：可见 → 全部隐藏；隐藏 → 只恢复悬浮条与组件面板。
/// 设置中心 / 弹出面板属于临时窗口，不随唤醒键自动弹出（用户要求）。
/// 常驻豁免：直播画中画（关 UI 继续看）与 FPS 悬浮窗（关 UI 继续显示帧数）。
const TOGGLE_EXEMPT: &[&str] = &["live-browser", "fps-overlay"];

/// 隐藏 UI 时可见的「可恢复窗口」快照（bar + widget-*）：唤醒时只恢复这些。
/// 组件面板现在是「隐藏复用」的常驻窗口（关闭不销毁），若唤醒时无条件 show
/// 全部 widget-*，用户从没点开/已关掉的面板也会被一并带出来。
static SHOWN_AT_HIDE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn toggle_ui_visibility(app: &AppHandle) {
    toggle_ui_visibility_with_focus(app, false);
}

/// force_focus=true：手柄 Xbox 键唤醒时用——用户按的是手柄，就是要来操作界面的，
/// 此时即使正在全屏也要把焦点拿过来（否则手柄事件不会路由到悬浮条）。
pub fn toggle_ui_visibility_with_focus(app: &AppHandle, force_focus: bool) {
    // 并发去抖：toggle 内部有阻塞重试（focus_window / wait_modifiers_released 最长几百 ms），
    // 期间若再来一次 toggle（快速连按、或热键重复），多个子线程会并发改同一批窗口的可见性，
    // 互相把对方刚设好的状态又改掉 → 「关不掉 / 关不干净」。上一次没跑完就直接跳过这一次。
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        log::info!("toggle：上一次切换尚未结束，忽略本次重入");
        return;
    }
    let _guard = {
        struct G;
        impl Drop for G {
            fn drop(&mut self) {
                BUSY.store(false, Ordering::Release);
            }
        }
        G
    };
    // 抢焦点前先记下「谁现在在前台」（通常是游戏）：界面收起时要原样还回去，
    // 否则 Windows 会顺手把前台交给 Z 序里的下一个窗口（跳桌面 / 任务视图）
    overlay::note_foreground();
    let bar_win = app.get_webview_window("bar");
    // 注意：最小化（Win+D 返回桌面会最小化所有窗口）在 Win32 里 IsWindowVisible 仍是 true，
    // 只判 is_visible() 会把「最小化」当成「已显示」→ 再点一次变成隐藏，UI 永远回不来。
    let hiding = bar_win
        .as_ref()
        .map(|w| {
            let vis = w.is_visible().unwrap_or(false);
            let min = w.is_minimized().unwrap_or(false);
            if min {
                log::info!("悬浮条处于最小化状态（visible={vis}）→ 本次为显示");
            }
            vis && !min
        })
        .unwrap_or(false);
    if bar_win.is_none() {
        log::warn!("显示/隐藏：找不到悬浮条窗口（bar）");
    }
    let mut was_overlay_session = false;
    if hiding {
        // 焦点先礼后兵：必须在 hide 之前把前台还给游戏。
        // 顺序反了的话，隐藏前台窗口会让 Windows 把前台塞给 Z 序里的下一个窗口，
        // 玩家看到的就是「退出悬浮条后突然跳到桌面 / 别的窗口 / 任务视图」。
        // 同时退出 GAMEBAR_INTERACTIVE：还原 NOACTIVATE 样式、放开手柄捕获。
        overlay::deactivate_gamebar(app, "收起界面");
        // 覆盖层会话结束 + 立刻清掉共享内存里的可见位：游戏画面马上恢复干净，
        // 不用等 worker 下一轮（40ms）才发现窗口不可见。窗口本身稍后、hide 之后再
        // 搬回屏幕内——先搬再隐会让真实窗口在游戏画面上闪一下。
        was_overlay_session = crate::overlay_inject::end_session_and_stop();
        if let Ok(mut snap) = SHOWN_AT_HIDE.lock() {
            snap.clear();
        }
    }
    // 覆盖层呼出判定：注入开关开 + 当前游戏已注入挂钩 → 界面直接画进游戏
    // 交换链，不显示任何真实窗口（topmost 窗口打断独占全屏独立翻转 = 画面闪顿，
    // 这是 Windows 合成规则，唯一解法就是注入式绘制）。
    // 就绪判定不满足时一律回退真实窗口路径——宁可画面动一下，呼不出界面更糟。
    let mut overlay_mode = false;
    if !hiding {
        if let Some(bar) = app.get_webview_window("bar") {
            if let Ok(pos) = bar.outer_position() {
                if let Ok(size) = bar.outer_size() {
                    if crate::overlay_inject::overlay_mode_ready(app) {
                        crate::overlay_inject::begin_ui_session(
                            pos.x,
                            pos.y,
                            size.width as i32,
                            size.height as i32,
                        );
                        overlay_mode = true;
                    }
                }
            }
        }
    }
    for (label, win) in app.webview_windows() {
        if TOGGLE_EXEMPT.contains(&label.as_str()) {
            continue;
        }
        if hiding {
            // 快照「本次真的会消失」的可恢复窗口（含最小化——IsWindowVisible 对
            // 最小化窗口仍为 true，is_visible() 同样判真）
            let was_up = win.is_visible().unwrap_or(false);
            if was_up && (label == "bar" || label.starts_with("widget-")) {
                if let Ok(mut snap) = SHOWN_AT_HIDE.lock() {
                    if !snap.contains(&label) {
                        snap.push(label.clone());
                    }
                }
            }
            let _ = win.hide();
            // 快捷抽屉不在快照/唤醒恢复之列（模态弹层不随唤醒弹出），但隐藏它必须
            // 通知悬浮条复位点亮态——否则 bar 的 quickExpanded 停在「开着」，
            // 唤醒后第一次点快捷按钮被当成「关闭」吞掉（表现为「打不开了」）
            if label == "quick-drawer" && was_up {
                let _ = app.emit("quick://closed", ());
            }
        } else {
            // 覆盖层模式：不显示任何真实窗口（bar 之外的 widget 面板保持隐藏；
            // 快照里的 widget 本来也是「上次收起时开着」的，为稳妥全部跳过）
            if overlay_mode {
                if label == "bar" {
                    // 悬浮条必须「可见但不在屏幕内」：可见 → webview 继续渲染，
                    // PrintWindow 才抓得到内容（worker 也按可见性决定要不要推帧）；
                    // 不在屏幕内 → 不用真实 topmost 窗口压着游戏，独立翻转不被打断。
                    let _ = win.unminimize();
                    overlay::force_show(&win);
                    crate::overlay_inject::park_bar_offscreen(app);
                    let _ = app.emit("bar://reveal", ());
                }
                continue;
            }
            let restored = label == "bar"
                || SHOWN_AT_HIDE
                    .lock()
                    .map(|snap| snap.contains(&label))
                    .unwrap_or(false);
            if !restored {
                continue;
            }
            let _ = win.unminimize();
            let _ = win.show();
            // 兜底：Tauri 的 show() 在最小化 / 某些隐藏态下不生效，直接走 Win32 显示 + 置顶
            overlay::force_show(&win);
            // 位置兜底：最小化会把窗口挪到 (-32000,-32000)，被记住再恢复就跑到屏幕外了
            overlay::ensure_onscreen(&win);
            if label == "bar" {
                let vis = win.is_visible().unwrap_or(false);
                let size = win
                    .inner_size()
                    .map(|s| format!("{}x{}", s.width, s.height))
                    .unwrap_or_else(|_| "?".into());
                let pos = win
                    .outer_position()
                    .map(|p| format!("({},{})", p.x, p.y))
                    .unwrap_or_else(|_| "?".into());
                log::info!("显示悬浮条：visible={vis} size={size} pos={pos}");
            }
        }
    }
    if hiding {
        // 窗口都已隐藏，现在把覆盖层会话期间挪出屏幕的悬浮条搬回屏幕内（顺序在这里
        // 很重要：先隐后搬，玩家看不到它闪；先搬后隐会看到一条真实窗口划过画面）
        if was_overlay_session {
            crate::overlay_inject::restore_bar_position(app);
        }
        log::info!("已隐藏软件界面（唤醒键）");
    } else if overlay_mode {
        // 覆盖层呼出：界面已画进游戏交换链。游戏必须保持前台（它还要收手柄输入
        // 来操作本界面），任何焦点切换/前台激活都会打断独立翻转（画面闪顿）。
        // 手柄路由靠「Xbox 键唤醒接管窗口」把输入导向 bar，无需焦点。
        crate::gamepad::note_wake();
        log::info!("已呼出软件界面（覆盖层模式：画进游戏画面，真实窗口未显示）");
    } else {
        // 悬浮条可能停在折叠态（只留屏幕上沿一条把手）：窗口可见但内容翻出去了，
        // 光 show() 看起来就是「点了没反应」。让前端自己展开。
        let _ = app.emit("bar://reveal", ());
        // 上面的显示一律走 SW_SHOWNA / SWP_NOACTIVATE，前台仍停在游戏上；
        // 只有确��要接管键鼠/手柄时才切前台 —— 全屏（尤其独占全屏）一丢前台
        // 就释放独占模式，表现就是画面切一下 / 黑一帧 / 自最小化。
        let fullscreen = overlay::foreground_is_fullscreen_app();
        let will_focus = force_focus || !fullscreen;
        let got = if will_focus {
            wait_modifiers_released(Duration::from_millis(400));
            if force_focus {
                overlay::activate_gamebar(app)
            } else {
                app.get_webview_window("bar")
                    .is_some_and(|b| overlay::focus_window(&b))
            }
        } else {
            false
        };
        // 排查「唤醒时游戏画面动了一下」直接看这一行：全屏=true 且 抢焦点=false
        // 时本软件没有动过前台，画面若仍动，那就是覆盖层本身打断独立翻转所致
        // （Windows 规则，只有注入式绘制能彻底避免）。
        log::info!("唤醒界面：前台全屏={fullscreen} 抢焦点={will_focus} 拿到前台={got}");
        if !got && fullscreen {
            // 抢焦点失败：游戏仍持有焦点，路由层已暂停手柄导航
            // （gamepad.rs 不再接管，避免按键同时进游戏）。告知用户原因。
            show_bar_tip(
                app,
                "未能取得游戏焦点：手柄控制已暂停（按键会同时进入游戏）",
            );
        }
        // 手柄接管窗口： Xbox 键路径在 gamepad.rs 已自开；键盘/托盘唤醒时焦点已强制
        // 给悬浮条，手柄输入走「可见+焦点」路由，无需再开接管（避免空转高频轮询）。
        log::info!("已显示软件界面（唤醒键）：仅悬浮条与组件");
    }
}

/// 借 tooltip 小窗在悬浮条下方弹一条提示（免焦点，不打断游戏）。
/// tooltip 4 秒自动消失，无需 hide 兜底。
fn show_bar_tip(app: &AppHandle, text: &str) {
    let Some(bar) = app.get_webview_window("bar") else {
        return;
    };
    let (Ok(pos), Ok(size)) = (bar.outer_position(), bar.outer_size()) else {
        return;
    };
    let _ = app.emit(
        "tip://show",
        serde_json::json!({
            "text": text,
            "x": pos.x + size.width as i32 / 2,
            "y": pos.y + size.height as i32 + 8,
        }),
    );
}

/// 阻塞等待物理修饰键全部松开（带超时兜底）。
/// 唤醒键是组合键：keydown 阶段修饰键已按原样进了前台窗口（游戏），
/// 焦点切换必须等玩家松手后做，否则双方都拿到半套按键状态。
/// settings.summonFocus：唤醒键呼出时是否立即接管键鼠焦点（默认开）。
/// 每次按键现读磁盘——唤醒是低频动作，config.json 极小，不值得为它单设缓存；
/// 设置中心改完下一次按键即生效。
fn summon_focus(app: &AppHandle) -> bool {
    tauri::async_runtime::block_on(async {
        crate::commands::load_config(app.clone())
            .await
            .ok()
            .and_then(|v| v["settings"]["summonFocus"].as_bool())
    })
    .unwrap_or(true)
}

fn wait_modifiers_released(timeout: Duration) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    let t0 = std::time::Instant::now();
    loop {
        let held = unsafe {
            [VK_LWIN, VK_RWIN, VK_CONTROL, VK_MENU, VK_SHIFT]
                .iter()
                .any(|vk| ((GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000) != 0)
        };
        if !held || t0.elapsed() >= timeout {
            return;
        }
        std::thread::sleep(Duration::from_millis(12));
    }
}
