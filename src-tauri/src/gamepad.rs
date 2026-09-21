//! Xbox 手柄支持：
//! - **Xbox 键（guide）**：标准 XInputGetState 不回报该键，用 XInputGetStateEx
//!   （xinput1_4 / xinput1_3 的 100 号导出，Game Bar 同款方式）后台轮询，按下沿
//!   触发 显示/隐藏全部界面（与唤醒键、托盘左键同路径）。
//! - **A 确认 / B 返回 / 左摇杆与十字键选择**：状态变化以 gamepad://input 广播，
//!   只发给「可见且有系统焦点」的窗口（其余窗口的门控+前端焦点导航双重过滤）。
//!   游戏在前台、拥有系统焦点时**不向任何 UI 窗口发送**——手柄完全交给游戏，
//!   杜绝「打游戏时 UI 跟着动 / 控制 UI 时两边抢输入」的双控问题。
//!
//! 限制：XInput 轮询是只读的，无法拦截输入——游戏在前台时游戏同样会收到手柄原始
//! 状态，因此「控制 UI 时游戏端不再收到按键」需要 HID 虚拟手柄驱动拦截（超出本应用
//! 范围，且不注入/不装驱动的 R1 红线）。进程内唯一能做的是靠焦点路由让 UI 独占导航。
//! 因此「接管窗口」在前台是全屏应用时直接放弃路由（见 poll_loop）：此时游戏持有
//! 焦点与手柄输入，若照常导航 UI，按键会同时进游戏（游戏内乱按键/弹菜单）。
//! 唤醒时若抢焦点失败（hotkeys.rs）会用 tooltip 告知用户手柄控制已暂停。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use windows::core::{s, PCSTR, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// 手柄控制是否启用（settings.gamepad，默认 true；save_config 时同步）
static ENABLED: AtomicBool = AtomicBool::new(true);

/// 手柄输入的显式所有者（框选两级导航模型）：
/// None = 无人持有（走旧的焦点/锁定回退路由）；
/// Some("bar"/"widget-audio"/...) = 框选层或元素层当前驻留的窗口，输入独占给它。
/// 由悬浮条（仲裁者）经 gamepad_set_owner 命令维护，见 bar/App.vue 的框选状态机。
static GAMEPAD_OWNER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 更新手柄输入所有者（label=None 清除）。JS 侧在框选切换/进入面板/退出时调用。
#[tauri::command]
pub fn gamepad_set_owner(label: Option<String>) {
    match GAMEPAD_OWNER.lock() {
        Ok(mut slot) => *slot = label.filter(|s| !s.is_empty()),
        Err(_) => log::warn!("gamepad_set_owner：所有者锁中毒，忽略"),
    }
}

fn gamepad_owner() -> Option<String> {
    GAMEPAD_OWNER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/* ---- 输入设备自动切换（鼠标 ↔ 手柄）----
 * 手柄侧是「锁定」模型（owner 独占路由），鼠标一动就该解锁，否则手柄握着归属、
 * 鼠标再怎么点都像没反应；反过来手柄一动要立刻锁回去。
 * 判定必须放在这个轮询线程里：解锁后若窗口都没有焦点，前端根本收不到手柄输入，
 * 也就没法自己切回手柄模式——而这里始终能看到手柄状态。 */
#[derive(Clone, Copy, PartialEq, Debug)]
enum InputMode {
    Mouse,
    Gamepad,
}
static INPUT_MODE: std::sync::Mutex<InputMode> = std::sync::Mutex::new(InputMode::Gamepad);

fn input_mode() -> InputMode {
    *INPUT_MODE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 鼠标一动即切回鼠标模式（悬浮条广播 input://mouse 时调用）。
#[tauri::command]
pub fn set_input_mode(mode: String) {
    let next = if mode.eq_ignore_ascii_case("mouse") {
        InputMode::Mouse
    } else {
        InputMode::Gamepad
    };
    if let Ok(mut slot) = INPUT_MODE.lock() {
        *slot = next;
    }
}

/// 手柄有实际输入 → 切回手柄模式。刚从鼠标切回来时广播一次，
/// 让前端把归属/框选状态锁回去（无需用户先点一下窗口）。
fn note_gamepad_activity(app: &AppHandle) {
    let flipped = match INPUT_MODE.lock() {
        Ok(mut slot) => {
            let was = *slot;
            *slot = InputMode::Gamepad;
            was == InputMode::Mouse
        }
        Err(e) => {
            let mut slot = e.into_inner();
            let was = *slot;
            *slot = InputMode::Gamepad;
            was == InputMode::Mouse
        }
    };
    if flipped {
        let _ = app.emit("input://gamepad", serde_json::json!({ "source": "rust" }));
    }
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XInputGamepad {
    w_buttons: u16,
    l_trigger: u8,
    r_trigger: u8,
    thumb_lx: i16,
    thumb_ly: i16,
    thumb_rx: i16,
    thumb_ry: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XInputState {
    packet: u32,
    gamepad: XInputGamepad,
}

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XInputState) -> u32;

/// 手柄按键位（XINPUT_GAMEPAD_*）
const BTN_DPAD_UP: u16 = 0x0001;
const BTN_DPAD_DOWN: u16 = 0x0002;
const BTN_DPAD_LEFT: u16 = 0x0004;
const BTN_DPAD_RIGHT: u16 = 0x0008;
const BTN_START: u16 = 0x0010;
const BTN_BACK: u16 = 0x0020;
const BTN_GUIDE: u16 = 0x0400;
const BTN_A: u16 = 0x1000;
const BTN_B: u16 = 0x2000;
/// X 键：界面已经开着时按它 = 「把控制权交给界面」（抢焦点，见 poll_loop）
const BTN_X: u16 = 0x4000;

/// 加载 XInputGetStateEx：依次尝试 xinput1_4 / xinput1_3 的具名导出与 100 号序号导出。
unsafe fn load_get_state_ex() -> Option<XInputGetStateFn> {
    for name in ["xinput1_4.dll", "xinput1_3.dll"] {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let module = match LoadLibraryW(PCWSTR(wide.as_ptr())) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // 具名导出（xinput1_4 有 XInputGetStateEx）
        if let Some(p) = GetProcAddress(module, PCSTR(s!("XInputGetStateEx").as_ptr())) {
            let f: XInputGetStateFn = std::mem::transmute(p);
            log::info!("XInputGetStateEx 已加载（{name} 具名导出）");
            return Some(f);
        }
        // 100 号序号导出（xinput1_3 的经典形式）
        if let Some(p) = GetProcAddress(module, PCSTR(100u64 as *const u8)) {
            let f: XInputGetStateFn = std::mem::transmute(p);
            log::info!("XInputGetStateEx 已加载（{name} #100）");
            return Some(f);
        }
    }
    None
}

/// 启动手柄轮询线程（应用生命周期常驻；禁用时低频空转）
/// 手柄唤醒后的「接管窗口」：唤醒后一段时间内，即使系统焦点被游戏/其他窗口
/// 拿走（手柄触发 SetForegroundWindow 可能被系统拒绝），摇杆/A/B 仍路由给悬浮条。
static LAST_WAKE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

const TAKEOVER_MS: u64 = 60_000;

/// 通知手柄线程「用户刚唤醒界面」：打开接管窗口（键盘唤醒键/托盘/Xbox 键共用）
pub fn note_wake() {
    if let Ok(mut slot) = LAST_WAKE.lock() {
        *slot = Some(std::time::Instant::now());
    }
}

fn wake_window_active() -> bool {
    LAST_WAKE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t.elapsed() < Duration::from_millis(TAKEOVER_MS))
        .unwrap_or(false)
}

fn extend_wake_window() {
    if let Ok(mut slot) = LAST_WAKE.lock() {
        *slot = Some(std::time::Instant::now());
    }
}

pub fn init(app: AppHandle) {
    // 启动即读一次配置（settings.gamepad 缺省开启），避免「上次禁用、重启后误生效」
    let on = std::fs::read_to_string(
        app.path()
            .app_data_dir()
            .unwrap_or_default()
            .join("config.json"),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|v| {
        v.get("settings")?
            .get("gamepad")?
            .as_bool()
    })
    .unwrap_or(true);
    ENABLED.store(on, Ordering::Relaxed);
    std::thread::spawn(move || poll_loop(app));
}

fn poll_loop(app: AppHandle) {
    let get_state = match unsafe { load_get_state_ex() } {
        Some(f) => f,
        None => {
            log::warn!("XInputGetStateEx 不可用，手柄控制未启用");
            return;
        }
    };
    // 各手柄槽位的上一次状态（buttons 原值 + 已量化的方向位）
    let mut prev_btn = [0u16; 4];
    let mut prev_dir = [0u8; 4];
    let mut state = XInputState::default();
    let mut connected = false;
    // Xbox 键去抖：同一物理按压的抖动不重复触发显隐切换（日志里见过 1 秒内双触发）
    let mut last_guide = std::time::Instant::now() - std::time::Duration::from_secs(1);
    // 无 Xbox 键手柄的替代唤醒：Start+Select 同按住 600ms = 等同 Xbox 键。
    // 各槽位记录组合键首次按下的时刻（None=未按）。
    let mut combo_at = [None::<std::time::Instant>; 4];
    // 触发锁：按住不放不连发，必须两键全松开才重新武装
    let mut combo_latched = [false; 4];

    loop {
        if !ENABLED.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
            prev_btn = [0; 4];
            prev_dir = [0; 4];
            combo_at = [None; 4];
            continue;
        }
        let mut any_connected = false;
        for slot in 0..4u32 {
            if unsafe { get_state(slot, &mut state) } != ERROR_SUCCESS.0 {
                prev_btn[slot as usize] = 0;
                prev_dir[slot as usize] = 0;
                combo_at[slot as usize] = None;
                continue;
            }
            any_connected = true;
            let g = &state.gamepad;
            let btn = g.w_buttons;

            // Xbox 键：按下沿 → 唤醒/隐藏全部界面（同唤醒键路径），500ms 去抖
            if (btn & BTN_GUIDE) != 0
                && (prev_btn[slot as usize] & BTN_GUIDE) == 0
                && last_guide.elapsed() > Duration::from_millis(500)
            {
                last_guide = std::time::Instant::now();
                log::info!("手柄 Xbox 键：切换界面显隐");
                note_wake();
                // force_focus=true：用户按的是手柄，就是要来操作界面的（哪怕正在全屏游戏）
                crate::hotkeys::toggle_ui_visibility_with_focus(&app, true);
            }

            // 无 Xbox 键手柄的替代唤醒：Start+Select（←Back/→Start 同按）持续 600ms
            // = 等同 Xbox 键。第三方 XInput 手柄（北通/莱仕达/廉价手柄）普遍不回报
            // guide 位（0x0400 恒为 0），Xbox 键路径永远走不到，游戏内无法呼出 UI。
            // 触发后须两键全松开才重新武装（防长按连发），并共享 guide 的去抖时间戳。
            let si = slot as usize;
            let combo_down = (btn & BTN_START) != 0 && (btn & BTN_BACK) != 0;
            if !combo_down {
                combo_at[si] = None;
                combo_latched[si] = false; // 两键全松开 → 重新武装
            } else if !combo_latched[si] {
                let since = *combo_at[si].get_or_insert(std::time::Instant::now());
                if since.elapsed() > Duration::from_millis(600)
                    && last_guide.elapsed() > Duration::from_millis(800)
                {
                    last_guide = std::time::Instant::now();
                    combo_latched[si] = true; // 按住不放不连发，松开后才可再触发
                    log::info!("手柄 Start+Select 长按：等同 Xbox 键，切换界面显隐");
                    note_wake();
                    crate::hotkeys::toggle_ui_visibility_with_focus(&app, true);
                }
            }

            // X 键：界面已经开着、但焦点还在游戏里时按一下 → 把焦点抢到界面。
            // 这是用户主动「交权」的入口：不打断没开界面时的游戏操作（UI 不可见就完全不理会）。
            if (btn & BTN_X) != 0
                && (prev_btn[slot as usize] & BTN_X) == 0
                && crate::overlay::ui_awake(&app)
                && !crate::overlay::foreground_is_ours()
                && crate::live_pad::mode() == crate::live_pad::MODE_GAME
            {
                log::info!("手柄 X 键：界面已打开 → 切到交互模式");
                note_gamepad_activity(&app); // 切回手柄模式 + 打开接管窗口
                // 完整激活（摘 NOACTIVATE → 前台 → 焦点 → WebView2 → 校验）
                let got = crate::overlay::activate_gamebar(&app);
                log::info!("手柄 X 键激活：{}", if got { "成功" } else { "失败（前台锁 / 游戏反抢）" });
            }

            // ---- 直播浏览器手柄控制（ControllerInputRouter）----
            // 控制态：本 tick 输入全部被 live_pad 消费（游戏侧由 DLL 拦截为静止手柄），
            // 不再走 UI 导航路由；锁定态：live_pad 只处理 B 键（语音/回注），
            // 其余照常（返回 false，UI 导航不受影响）。
            if crate::live_pad::tick(
                &app,
                btn,
                g.l_trigger,
                g.r_trigger,
                g.thumb_lx,
                g.thumb_ly,
                g.thumb_rx,
                g.thumb_ry,
            ) {
                prev_btn[slot as usize] = btn;
                prev_dir[slot as usize] = 0;
                continue;
            }

            // 方向量化：十字键 + 左摇杆（死区 50%）合并成 4 向位掩码
            let lx = g.thumb_lx as i32;
            let ly = g.thumb_ly as i32;
            let mut dir = 0u8;
            if (btn & BTN_DPAD_UP) != 0 || (ly.abs() > 16384 && ly > 0) {
                dir |= 1;
            }
            if (btn & BTN_DPAD_DOWN) != 0 || (ly.abs() > 16384 && ly < 0) {
                dir |= 2;
            }
            if (btn & BTN_DPAD_LEFT) != 0 || (lx.abs() > 16384 && lx < 0) {
                dir |= 4;
            }
            if (btn & BTN_DPAD_RIGHT) != 0 || (lx.abs() > 16384 && lx > 0) {
                dir |= 8;
            }

            let cur = btn & (BTN_A | BTN_B | BTN_START);
            let prev_cur = prev_btn[slot as usize] & (BTN_A | BTN_B | BTN_START);
            // 输入设备自动切换：手柄一有实际输入就夺回控制权（含从鼠标模式切回）
            if dir != 0 || cur != 0 {
                note_gamepad_activity(&app);
            }
            if dir != prev_dir[slot as usize] || cur != prev_cur {
                let payload = serde_json::json!({
                    "d": dir,
                    "a": (cur & BTN_A) != 0,
                    "b": (cur & BTN_B) != 0,
                    "menu": (cur & BTN_START) != 0,
                    "slot": slot,
                });
                // 路由：只发给「可见且有系统焦点」的窗口（焦点门控天然分流多窗口）。
                // 没有任何窗口聚焦（即游戏在前台、拥有系统焦点）时，不向 UI 发送任何手柄
                // 事件——手柄完全交给游戏，避免「打游戏时 UI 也跟着动」（#2）。
                // 唤醒界面时 toggle_ui_visibility（Xbox 键路径）已把焦点给悬浮条，所以唤醒后
                // 悬浮条即成为焦点窗口、自然承接手柄导航，无需无脑回退到 bar。
                // 键盘热键在有全屏前台窗口时不抢焦点，此时界面只是浮着，用户点进来才会接管。
                let mut target: Option<String> = None;
                // 二级界面锁定（最优先）：任一 模态弹层/抽屉/工具条 打开中 →
                // 手柄独占锁定给它，必须先退出（B）才能控制悬浮条。
                // 此判定先于「焦点窗口」：点开抽屉后焦点仍在悬浮条，
                // 若按焦点路由，手柄会窜回悬浮条、永远无法操作抽屉。
                // 注意组件面板（widget-*）不在锁定列表：它们按框选两级导航模型
                // 由 GAMEPAD_OWNER 显式持有（B 返回不关面板、可多开并存）。
                if target.is_none() {
                    for l in [
                        "flyout-games",
                        "flyout-settings",
                        "settings",
                        "live-toolbar",
                        "quick-drawer",
                    ] {
                        if app
                            .get_webview_window(l)
                            .map(|w| w.is_visible().unwrap_or(false))
                            .unwrap_or(false)
                        {
                            target = Some(l.to_string());
                            break;
                        }
                    }
                }
                // 显式所有者（框选层/元素层驻留窗口）：优先于焦点回退——
                // 组件面板是 NOACTIVATE 窗口，没有系统焦点也必须能被手柄操作；
                // 持有期间「只能操作这一个界面」，其余窗口（含悬浮条）不收手柄输入
                if target.is_none() {
                    if let Some(owner) = gamepad_owner() {
                        let visible = app
                            .get_webview_window(&owner)
                            .map(|w| w.is_visible().unwrap_or(false))
                            .unwrap_or(false);
                        if visible {
                            target = Some(owner);
                            extend_wake_window();
                        }
                    }
                }
                // 无二级界面时：谁有系统焦点谁接收（用户正在操作的窗口）
                if target.is_none() {
                    for (label, win) in app.webview_windows() {
                        if win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false) {
                            target = Some(label);
                            break;
                        }
                    }
                }
                // 手柄唤醒接管窗口：Xbox 键唤醒后即使焦点被拿走，摇杆/A/B 仍控制
                // 悬浮条；每次实际路由都会续期，停止操作 60s 后控制权交还游戏。
                // 前台是全屏应用时放弃接管（游戏持有焦点、同样收到原始按键，
                // 「两边一起反应」）。
                if target.is_none() {
                    let bar_visible = app
                        .get_webview_window("bar")
                        .map(|w| w.is_visible().unwrap_or(false))
                        .unwrap_or(false);
                    if bar_visible
                        && (
                            // GAMEBAR_INTERACTIVE（手柄快捷键唤出且前台已在我们这儿）：
                            // 手柄导航归界面，不再看「唤醒窗口期」——用户刚明确按了键要操作菜单
                            crate::overlay::gamepad_capture()
                            || (wake_window_active()
                                && !crate::overlay::foreground_is_fullscreen_app()))
                    {
                        target = Some("bar".to_string());
                        extend_wake_window();
                    }
                }
                // 诊断：把每次路由结果写进日志（排查「手柄没反应」时一眼看出谁收走了输入）
                log::info!(
                    "[gp] d={dir} a={} b={} mode={:?} → target={:?}",
                    (cur & BTN_A) != 0,
                    (cur & BTN_B) != 0,
                    input_mode(),
                    target
                );
                // 鼠标模式期间不把手柄输入发给界面（用户正在用鼠标操作）。
                // 上面已记下手柄活动并切回手柄模式，所以按下的这一下就能正常接管，
                // 不会出现「切到鼠标后手柄彻底失灵」。
                if input_mode() == InputMode::Gamepad {
                    if let Some(target) = target {
                        let _ = app.emit_to(&target, "gamepad://input", payload);
                    }
                }
            }
            prev_btn[slot as usize] = btn;
            prev_dir[slot as usize] = dir;
        }
        // 自适应轮询频率：
        // - 无手柄：250ms（插上手柄 250ms 内即可用）
        // - 界面可见 + 手柄导航活跃（接管窗口期内有输入）：16ms（导航跟手）
        // - 界面可见但手柄闲置：60ms——XInputGetState 会与游戏自己的手柄调用争用，
        //   UI 开着的全程 16ms 狂轮询是「游戏卡顿」元凶；一旦手柄有输入
        //   （路由时 extend_wake_window）立刻回到 16ms
        // - 界面隐藏：250ms（只保留 Xbox 键唤醒检测）
        let ui_awake = app
            .get_webview_window("bar")
            .map(|w| {
                w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false)
            })
            .unwrap_or(false);
        // 直播窗口可见：手柄随时可能自动进控制态（live_pad::try_auto_enter），
        // 250ms 轮询会漏掉轻点沿，必须 16ms 跟手
        let live_visible = app
            .get_webview_window("live-browser")
            .map(|w| w.is_visible().unwrap_or(false))
            .unwrap_or(false);
        let idle = if !any_connected {
            Duration::from_millis(250)
        } else if crate::live_pad::mode() == crate::live_pad::MODE_CONTROL {
            // 控制态：虚拟鼠标/滚动/快进退全在 tick 里逐帧算，必须高频跟手
            Duration::from_millis(16)
        } else if crate::live_pad::mode() == crate::live_pad::MODE_LOCKED {
            // 锁定态：宿主侧只消费 B 键边沿（短按回注 / 长按 500ms 进语音），
            // 对轮询精度不敏感。XInputGetState 会与游戏自己的手柄调用争用，
            // 全程 16ms 狂轮询是「游戏卡顿/偶发卡死」元凶 → 降到 40ms，
            // 既保住 B 长按阈值判定，又把与游戏争用 XInput 的频率砍到 1/2.5。
            Duration::from_millis(40)
        } else if live_visible {
            // MODE_GAME 但直播窗口开着：随时可能自动进控制态（try_auto_enter
            // 靠摇杆/ABY 沿判定），250ms 会漏轻点 → 16ms
            Duration::from_millis(16)
        } else if ui_awake {
            if wake_window_active() {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(60)
            }
        } else {
            Duration::from_millis(250)
        };
        if !any_connected && connected {
            log::info!("手柄已断开");
        }
        connected = any_connected;
        std::thread::sleep(idle);
    }
}
