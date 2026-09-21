//! 直播浏览器手柄控制模块（ControllerInputRouter / CursorRouter / VirtualCursor /
//! BrowserController / BrowserWindowController 的宿主侧总入口）。
//!
//! 三态：
//! - MODE=0 GAME_MODE：手柄归游戏，本模块完全旁路。
//! - MODE=1 BROWSER_MODE（含 WINDOW_MOVE / DRAG 子态）：左摇杆=虚拟鼠标、A=左键、
//!   A+摇杆=拖动、LT+摇杆=移窗、LT/RT=缩放、右摇杆=滚动/快进退、Y=播放暂停、
//!   B 短按返回 / 长按语音。此态下通过注入 DLL 的 XInput 拦截（overlay_inject::
//!   set_gp_block）让游戏**收不到**手柄输入——XInput 是进程轮询模型，与 Windows
//!   焦点无关，宿主侧无法拦截，唯一可靠落点是游戏进程内的 egb_hook.dll。
//! - MODE=2 LOCKED（锁定观看）：游戏正常玩（仅 B 被本模块消费）：
//!   短按 → 通过拦截位回注一颗干净的 B 点按给游戏；长按 500ms → 语音
//!   （Listening 期间 B 保持拦截，游戏收不到）。
//!
//! 光标共存（CursorRouter）：每 tick 比对真实光标位置与我们上次注入的位置，
//! 偏差 = 物理鼠标动了 → 交还控制权（Physical）；摇杆再动 → 从当前真实位置接管
//! （不传送，避免跳变）。拖动（A 按住）期间锁定为手柄，物理鼠标不抢。
//! 不依赖 SetForegroundWindow/WebView2.Focus 解决输入归属；焦点只在进入控制态时
//! 给一次浏览器（键盘回退 Space 需要），之后输入全部走逻辑路由。

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Listener, Manager};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetCursorPos, GetSystemMetrics, LoadCursorW, SetCursor, WindowFromPoint, GA_ROOT,
    IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_WAIT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub const MODE_GAME: u8 = 0;
pub const MODE_CONTROL: u8 = 1;
pub const MODE_LOCKED: u8 = 2;

/// XINPUT_GAMEPAD 按键位
const BTN_A: u16 = 0x1000;
const BTN_B: u16 = 0x2000;
const BTN_Y: u16 = 0x8000;
/// X 键（0x4000）：控制态下 = 切换锁定/解锁。全屏（纯净模式）时工具条被隐藏，
/// 导航栏上的锁按钮根本够不到，手柄需要一个不依赖工具条的入口。
/// 注：X 在 MODE_GAME 下另有用途（界面已开时抢焦点，见 gamepad.rs），
/// 但控制态下 tick 已经把输入全部消费掉，两边不会打架。
const BTN_X: u16 = 0x4000;

static MODE: AtomicU8 = AtomicU8::new(MODE_GAME);
/// 语音进行中标记（voice 命令入口防重入）
pub static VOICE_BUSY: AtomicU8 = AtomicU8::new(0);
/// 摇杆灵敏度（×100 定点缓存，0=未初始化）
static SENS: AtomicU32 = AtomicU32::new(0);

static APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

fn app_handle() -> Option<&'static AppHandle> {
    APP.get()
}

/* ============================== 摇杆与光标参数 ============================== */

const DEAD: f32 = 0.12; // 死区（规格 0.10~0.15）
const BASE_SPEED: f64 = 1300.0; // 虚拟鼠标满偏速度（物理像素/秒）
const EXP: f64 = 1.15; // 非线性指数（pow(abs,EXP)）
const B_HOLD: Duration = Duration::from_millis(500); // 长按进语音阈值
const B_TAP_INJECT: Duration = Duration::from_millis(80); // 锁定态回注 B 点按宽度
/// 工具条与内容窗的最大贴合间隙（物理像素）：超过就算「没贴合」，不纳入光标活动范围
const TOOLBAR_GAP: i32 = 24;

/// 摇杆满量程自适应：跟踪「近期实际最大推程」（任一轴、任一摇杆的原始绝对值）。
/// Xbox 360 等老手柄电位器行程短、常推不到 32767，若固定按满量程归一化，
/// 手柄推到底也只到 ~70% → 光标「非常慢」。这里用实测峰值当分母，任何手柄
/// 推到底都等于满速。松手后峰值缓慢衰减回落，防止噪声把分母越抬越小。
static STICK_PEAK: AtomicU32 = AtomicU32::new(32767);
/// 峰值下限：低于此值视为静止漂移，不采信（避免把微小噪声放大成满速）
const PEAK_MIN: u32 = 16000;

/// 每 tick 更新满量程峰值：取当前最大绝对值与「上次峰值×衰减」的较大者。
fn update_stick_peak(lx: i16, ly: i16, rx: i16, ry: i16) {
    let decay = 0.993f32; // ~16ms/tick，松手约 1.5s 回落到半程
    let v = [lx, ly, rx, ry]
        .iter()
        .map(|a| a.unsigned_abs() as u32)
        .max()
        .unwrap_or(0);
    let prev = STICK_PEAK.load(Ordering::Relaxed) as f32;
    let next = (v as f32).max(prev * decay).clamp(PEAK_MIN as f32, 32767.0);
    STICK_PEAK.store(next as u32, Ordering::Relaxed);
}

fn stick_norm(v: i16) -> f32 {
    // 分母用实测峰值（而非固定 32767），让短行程手柄推到底 = 满偏。
    let peak = STICK_PEAK.load(Ordering::Relaxed).max(PEAK_MIN) as f32;
    let n = v as f32 / peak;
    if n.abs() <= DEAD {
        0.0
    } else {
        (n.abs() - DEAD) / (1.0 - DEAD) * n.signum()
    }
}

/// 非线性速度曲线：sign(n) * |n|^EXP
fn shaped(n: f32) -> f64 {
    (n.abs() as f64).powf(EXP) * n.signum() as f64
}

/* ============================== 状态 ============================== */

#[derive(Clone, Copy, PartialEq)]
enum CursorOwner {
    Physical,
    Controller,
}

struct Pad {
    vx: f64,
    vy: f64,
    owner: CursorOwner,
    last_set: (i32, i32),
    moved_this_tick: bool,
    a_prev: bool,
    y_prev: bool,
    dragging: bool,
    /// 拖动期间「页面内指针移动」的上次派发时刻（节流用）
    drag_eval_at: Option<Instant>,
    /// 本次 A 按下落在哪个窗口上（live-browser / live-toolbar）：
    /// 松开与拖动的移动事件必须发回同一个窗口，否则工具条会收到"没有按下"的
    /// mouseup/click 而误触发导航栏按钮。
    a_target: Option<String>,
    /// X 键的上次状态：按下沿 = 锁定/解锁切换
    x_prev: bool,
    /// 上次问页面「光标该用什么形状」的时刻（节流）
    cursor_ask_at: Option<Instant>,
    /// 上次重申系统光标形状的时刻（游戏/系统可能把形状改回去）
    cursor_apply_at: Option<Instant>,
    scroll: f64,
    seek_at: Option<Instant>,
    inject_until: Option<Instant>,
    last_tick: Option<Instant>,
    nav_depth: i32,
    back_pending: bool,
}

impl Default for Pad {
    fn default() -> Self {
        Self {
            vx: 0.0,
            vy: 0.0,
            owner: CursorOwner::Physical,
            last_set: (0, 0),
            moved_this_tick: false,
            a_prev: false,
            y_prev: false,
            dragging: false,
            drag_eval_at: None,
            a_target: None,
            x_prev: false,
            cursor_ask_at: None,
            cursor_apply_at: None,
            scroll: 0.0,
            seek_at: None,
            inject_until: None,
            last_tick: None,
            nav_depth: 0,
            back_pending: false,
        }
    }
}

fn pad() -> &'static Mutex<Pad> {
    static P: std::sync::OnceLock<Mutex<Pad>> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(Pad::default()))
}

impl Pad {
    /// 当前虚拟光标位置：本 tick 刚注入过就用注入值（那是屏幕上真正的位置），
    /// 否则用读到的真实光标（物理鼠标可能动过）。两个都不是时退回上次注入点。
    fn cursor_now(&self, real: Option<(i32, i32)>) -> (i32, i32) {
        if self.moved_this_tick {
            self.last_set
        } else {
            real.unwrap_or(self.last_set)
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn mode() -> u8 {
    MODE.load(Ordering::Relaxed)
}

/// live_chrome 的 on_navigation 回调：页面导航时记账（B 返回链的历史深度）。
pub fn note_nav() {
    let mut p = lock(pad());
    if p.back_pending {
        p.back_pending = false;
        p.nav_depth = (p.nav_depth - 1).max(0);
    } else {
        p.nav_depth += 1;
    }
}

/// B 键需要执行的边沿事件（每个 tick 由 [`b_poll`] 产出一次）。
enum BEvent {
    None,
    /// 按下沿：立刻开录（阈值前的话也要录进去）
    Press,
    /// 长按达阈值：亮 HUD「正在听…」
    Promote,
    /// 正式松手且此前已进入语音：停止录音并识别
    VoiceEnd,
    /// 短按（没到长按阈值就松手）：丢弃这段录音
    ShortTap,
}

/// B 键的按下/长按状态。**刻意不放进 [`Pad`]**：`livepad_set_mode` 切模式时会把
/// `Pad` 整体重置（虚拟光标几何要按新模式重算），但 B 的状态必须跨模式保留——
/// 否则用户长按 B 说话期间只要发生一次模式切换（自动进控制态、短按返回链降级到
/// GAME、锁定/解锁…），「已按下多久 / 是否已进语音」就被清零：松手时被当成短按
/// → `voice::cancel()` 丢掉整段录音、还多走一次返回链，表现为「B 还按着，录音却被取消了」。
struct BState {
    /// 去抖后的按下状态
    down: bool,
    /// 连续读到「抬起」的 tick 数（去抖用）
    up_ticks: u8,
    down_at: Option<Instant>,
    /// 已长按过阈值、进入语音
    voice: bool,
}

fn b_state() -> &'static Mutex<BState> {
    static B: std::sync::OnceLock<Mutex<BState>> = std::sync::OnceLock::new();
    B.get_or_init(|| {
        Mutex::new(BState {
            down: false,
            up_ticks: 0,
            down_at: None,
            voice: false,
        })
    })
}

/// 抬起去抖帧数：连续这么多 tick 读到「抬起」才算真的松手。游戏同时在轮询同一个
/// 手柄，XInput 偶发单帧抖动不该打断正在进行的录音（日志里录音总在 1~2 秒被截断）。
const B_RELEASE_TICKS: u8 = 4; // ≈64ms @16ms/tick

/// 吃一次 B 的原始读数，返回本 tick 要执行的边沿事件。按下沿立即生效（不拖延起录）。
fn b_poll(raw: bool) -> BEvent {
    let mut s = lock(b_state());
    if raw {
        if s.up_ticks > 0 {
            log::info!(
                "[livepad] B 抖动已忽略：抬起只持续 {} 帧（需 {} 帧），录音不中断",
                s.up_ticks,
                B_RELEASE_TICKS
            );
        }
        s.up_ticks = 0;
        if !s.down {
            s.down = true;
            s.down_at = Some(Instant::now());
            s.voice = false;
            return BEvent::Press;
        }
        if !s.voice && s.down_at.map(|t| t.elapsed() >= B_HOLD).unwrap_or(false) {
            s.voice = true;
            return BEvent::Promote;
        }
        return BEvent::None;
    }
    if !s.down {
        return BEvent::None;
    }
    s.up_ticks = s.up_ticks.saturating_add(1);
    if s.up_ticks < B_RELEASE_TICKS {
        return BEvent::None;
    }
    s.down = false;
    s.up_ticks = 0;
    let held = s.down_at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
    s.down_at = None;
    let voice = std::mem::replace(&mut s.voice, false);
    log::info!(
        "[livepad] B 松手：按住 {:.2}s，{}",
        held.as_secs_f32(),
        if voice { "走语音识别" } else { "判定为短按" }
    );
    if voice {
        BEvent::VoiceEnd
    } else {
        BEvent::ShortTap
    }
}

/* ============================== 对外命令 ============================== */

/// 设置手柄控制模式。mode=1 要求注入链就绪（游戏内 XInput 拦截可用），否则拒绝——
/// 拦不住游戏输入还开手柄控制 = 两边同时消费，规格红线。
#[tauri::command]
pub fn livepad_set_mode(app: AppHandle, m: u8) -> Result<(), String> {
    match m {
        MODE_GAME => {}
        MODE_CONTROL => {
            if !crate::overlay_inject::gamepad_bridge_ready() {
                // 带上真实失败原因：只说「请开启注入/加入游戏库」在注入其实已经开着、
                // 游戏也在库里时完全是误导（例如游戏以管理员身份运行、本程序未提权）。
                let why = crate::overlay_inject::inject_error();
                return Err(if why.is_empty() {
                    "手柄控制需要游戏内覆盖层：请在设置中开启 DLL 注入，并把当前游戏加入游戏库"
                        .into()
                } else {
                    format!("手柄控制需要游戏内覆盖层，但注入没成功：{why}")
                });
            }
            if app.get_webview_window("live-browser").is_none() {
                return Err("直播浏览器未打开".into());
            }
        }
        MODE_LOCKED => {}
        other => return Err(format!("未知模式：{other}")),
    }
    let prev = MODE.swap(m, Ordering::SeqCst);
    if prev != m {
        let depth = {
            let mut p = lock(pad());
            let d = p.nav_depth;
            *p = Pad::default();
            p.nav_depth = d;
            d
        };
        let _ = depth;
        if m == MODE_CONTROL {
            gp_enter_control_idle();
            if let Some(w) = app.get_webview_window("live-browser") {
                // 清除可能残留的点击穿透：锁定态设过 WS_EX_TRANSPARENT，解锁走前端
                // UI 流程，而自动进控制态绕过了它——穿透挂着时窗口收不到
                // WM_MOUSEMOVE/点击，表现为「光标移到目标上不亮、A 键点不动」。
                let _ = crate::overlay::set_click_through(app.clone(), "live-browser".into(), false);
                {
                    let a = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = crate::live_chrome::live_cursor_hidden(a, false).await;
                    });
                }
                let _ = w.set_focus();
                // WebView2 只把鼠标**按键**消息派发给活动窗口：不在前台时注入的左键
                // 会被当成「激活点击」吞掉（实测 move/wheel 进得了页面、mousedown/up
                // 进不去）→ A 键点了没反应、拖动失效。所以这里要真正拿到前台，
                // 被前台锁挡下时做一次瞬时 Alt 解锁重试（overlay::force_foreground）。
                let got_fg = crate::overlay::force_foreground(&w);
                log::info!(
                    "[livepad] 进入控制态：浏览器前台={}",
                    if got_fg { "YES" } else { "NO（前台锁/游戏反抢，A 键可能失灵）" }
                );
                // 锁定时前端 hide 了工具条（live-toolbar）；进控制态要把它显示回来，
                // 否则虚拟光标够到了工具条区域却看不见（导航栏「消失」）。
                if let Some(tb) = app.get_webview_window("live-toolbar") {
                    let _ = tb.show();
                }
                // 把虚拟光标初始化到浏览器窗口中心并立即传送真实光标过去：
                // 否则用户没拨摇杆就直接按 A 时，光标还停在游戏画面上，
                // SendInput 的左键全点进游戏窗口（表现为「A 键点击失效」）。
                let rect = (|| -> Option<(i32, i32, i32, i32)> {
                    let pos = w.outer_position().ok()?;
                    let size = w.outer_size().ok()?;
                    Some((pos.x, pos.y, size.width as i32, size.height as i32))
                })();
                if let Some((bx, by, bw, bh)) = rect {
                    let (cx, cy) = (bx + bw / 2, by + bh / 2);
                    send_cursor(cx, cy);
                    let mut p = lock(pad());
                    p.vx = cx as f64;
                    p.vy = cy as f64;
                    p.owner = CursorOwner::Controller;
                    p.last_set = (cx, cy);
                }
            }
        }
        gp_apply();
        // 光标形状：进控制态先复位成箭头（随后由页面按 CSS 接管）；回游戏态也复位，
        // 免得把 I 形/手型留在真实鼠标上。**锁定态不动**——那态页面压着 cursor:none
        // 且前台已还给游戏，硬设箭头会把"锁定后指针不再浮在画面上"弄回原样。
        if m != MODE_LOCKED {
            CURSOR_KIND.store(0, Ordering::Relaxed);
            CURSOR_APPLIED.store(9, Ordering::Relaxed);
            apply_cursor(&app, true);
        }
        let _ = app.emit("livepad://mode", m);
        log::info!("[livepad] 模式 {prev} → {m}");
    }
    Ok(())
}

/// 当前状态（工具条按钮态同步用）
#[tauri::command]
pub fn livepad_status() -> serde_json::Value {
    serde_json::json!({
        "mode": mode(),
        "injectReady": crate::overlay_inject::gamepad_bridge_ready(),
        "voice": VOICE_BUSY.load(Ordering::Relaxed) != 0,
    })
}

/* ============================== 注入侧拦截桥 ============================== */

/// 按当前模式 + 回注窗口，把拦截档位写进游戏进程（见 overlay_inject::gp_set 注释）。
/// 每 tick 直接写：共享内存写入开销可忽略，且天然覆盖「换注入目标后视图重建」——
/// 新视图的 gp_en 默认 0，靠持续写恢复档位，不做缓存（缓存会漏掉换进程）。
fn gp_apply() {
    let (en, inj) = match mode() {
        MODE_CONTROL => (1u32, 0u32),
        MODE_LOCKED => {
            let inject = {
                let p = lock(pad());
                p.inject_until.map(|t| Instant::now() < t).unwrap_or(false)
            };
            (2, inject as u32)
        }
        _ => (0, 0),
    };
    crate::overlay_inject::gp_set(en, inj);
}

/// 进入控制态：写一次「4 槽位全静止」的合成状态（en=1 时游戏读到的就是它）。
/// result=0 + 全零 = 手柄已连接但没人按；未连接槽位保持 1167 让游戏正常降级。
fn gp_enter_control_idle() {
    let mut slots = [crate::overlay_inject::GpSlot {
        result: 0,
        packet: 0,
        buttons: 0,
        lt: 0,
        rt: 0,
        lx: 0,
        ly: 0,
        rx: 0,
        ry: 0,
    }; 4];
    slots[1..].iter_mut().for_each(|s| s.result = 1167);
    crate::overlay_inject::gp_write_slots(&slots);
}

/* ============================== 主 tick（gamepad.rs 轮询线程驱动） ============================== */

/// 每轮询 tick 调用一次（16ms）。返回 true = 本 tick 手柄输入已被本模块消费，
/// gamepad.rs 不再走 UI 导航路由。
pub fn tick(app: &AppHandle, btn: u16, lt8: u8, rt8: u8, lx: i16, ly: i16, rx: i16, ry: i16) -> bool {
    // 满量程自适应：先更新峰值，本 tick 所有 stick_norm（含自动进控制态检测）都用它
    update_stick_peak(lx, ly, rx, ry);
    match mode() {
        MODE_CONTROL => {
            control_tick(app, btn, lt8, rt8, lx, ly, rx, ry);
            true
        }
        MODE_LOCKED => {
            locked_tick(app, btn);
            false
        }
        _ => {
            // 自动进入控制态：直播窗口可见时，摇杆出死区 / A / B / Y 任一输入
            // 即视为「要操作浏览器」，无需先点工具条 🎮 按钮。
            if try_auto_enter(app, btn, lx, ly) {
                control_tick(app, btn, lt8, rt8, lx, ly, rx, ry);
                return true;
            }
            false
        }
    }
}

/// MODE_GAME 下检测控制意图并自动切到 CONTROL。注入未就绪时不自动进
/// （拦不住游戏输入 = 两边同控，规格红线），失败原因限频 10s 写一次日志。
fn try_auto_enter(app: &AppHandle, btn: u16, lx: i16, ly: i16) -> bool {
    let lxn = stick_norm(lx);
    let lyn = stick_norm(ly);
    let stick = (lxn * lxn + lyn * lyn).sqrt() > DEAD;
    let press = btn & (BTN_A | BTN_B | BTN_Y) != 0;
    if !stick && !press {
        return false;
    }
    let Some(w) = app.get_webview_window("live-browser") else {
        return false;
    };
    if !w.is_visible().unwrap_or(false) {
        return false;
    }
    match livepad_set_mode(app.clone(), MODE_CONTROL) {
        Ok(()) => {
            log::info!("[livepad] 检测到摇杆/ABY 输入 → 自动进入手柄控制");
            true
        }
        Err(e) => {
            static LAST: std::sync::OnceLock<Mutex<Option<Instant>>> = std::sync::OnceLock::new();
            let m = LAST.get_or_init(|| Mutex::new(None));
            let mut g = lock(m);
            if g.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true) {
                *g = Some(Instant::now());
                log::warn!("[livepad] 自动进入手柄控制失败：{e}");
            }
            false
        }
    }
}

fn dt(p: &mut Pad) -> f64 {
    let now = Instant::now();
    let d = p.last_tick.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.016);
    p.last_tick = Some(now);
    d.min(0.1)
}

/// 限频告警：同一条信息 10s 内最多写一次（光标每 16ms 一次 tick，直接 warn 会刷爆日志）
fn warn_10s(msg: &str) {
    static LAST: std::sync::OnceLock<Mutex<Option<Instant>>> = std::sync::OnceLock::new();
    let m = LAST.get_or_init(|| Mutex::new(None));
    let mut g = lock(m);
    if g.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true) {
        *g = Some(Instant::now());
        log::warn!("{msg}");
    }
}

/// 两个矩形是否「贴合」：某一轴上范围重叠，另一轴间隙不超过 [`TOOLBAR_GAP`]。
/// （工具条正常贴在内容窗正上方：x 范围重叠、y 间隙 0）
fn rects_adjacent(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let (ax0, ay0, ax1, ay1) = (a.0, a.1, a.0 + a.2, a.1 + a.3);
    let (bx0, by0, bx1, by1) = (b.0, b.1, b.0 + b.2, b.1 + b.3);
    let overlap_x = ax0 < bx1 && bx0 < ax1;
    let overlap_y = ay0 < by1 && by0 < ay1;
    let gap_x = (bx0 - ax1).max(ax0 - bx1);
    let gap_y = (by0 - ay1).max(ay0 - by1);
    (overlap_x && gap_y <= TOOLBAR_GAP) || (overlap_y && gap_x <= TOOLBAR_GAP)
}

/// 把点夹进「若干矩形的并集」：不在任何矩形内就投影到最近矩形里。
/// 保证虚拟光标永远落在我们自己的窗口上——否则光标会悬在游戏画面上
/// （hover 不亮、点击/滚轮全进游戏）。
fn clamp_to_region(x: f64, y: f64, rects: &[(i32, i32, i32, i32)]) -> (f64, f64) {
    let inside = rects.iter().any(|r| {
        x >= r.0 as f64 && x <= (r.0 + r.2) as f64 && y >= r.1 as f64 && y <= (r.1 + r.3) as f64
    });
    if inside {
        return (x, y);
    }
    let mut best = (x, y);
    let mut best_d = f64::MAX;
    for r in rects {
        let px = x.clamp(r.0 as f64, (r.0 + r.2) as f64);
        let py = y.clamp(r.1 as f64, (r.1 + r.3) as f64);
        let d = (px - x) * (px - x) + (py - y) * (py - y);
        if d < best_d {
            best_d = d;
            best = (px, py);
        }
    }
    best
}

/* ---------------- 控制态 ---------------- */

fn control_tick(
    app: &AppHandle,
    btn: u16,
    lt8: u8,
    rt8: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
) {
    if app.get_webview_window("live-browser").is_none() {
        let _ = livepad_set_mode(app.clone(), MODE_GAME);
        return;
    }
    // 控制态：拦截桥已在 set_mode 时置 en=1，每 tick 保持（防换注入目标丢档）
    gp_apply();

    let Some(win) = app.get_webview_window("live-browser") else { return };
    let rect = (|| -> Option<(i32, i32, i32, i32)> {
        let pos = win.outer_position().ok()?;
        let size = win.outer_size().ok()?;
        Some((pos.x, pos.y, size.width as i32, size.height as i32))
    })();
    let (bx, by, bw, bh) = rect.unwrap_or((0, 0, 800, 450));
    let content = (bx, by, bw, bh);
    // 虚拟光标活动范围 = live-browser（内容窗）∪ live-toolbar，**工具条只在真的
    // 贴住内容窗时才纳入**。旧实现直接取两者外接矩形：锁定态曾把工具条 hide 掉、
    // 解锁/进控制态只 show 不重新贴合，工具条停在旧位置时外接框里就出现一大片
    // 「两个窗口都不在」的死区 —— 光标落在死区上等于悬在游戏画面上：hover 不亮、
    // A 点击与右摇杆滚轮全进游戏（本次「左摇杆不亮、A/右摇杆失效」根因之一）。
    let mut rects: Vec<(i32, i32, i32, i32)> = vec![content];
    let mut tb_rect: Option<(i32, i32, i32, i32)> = None;
    if let Some(tb) = app.get_webview_window("live-toolbar") {
        if let (Ok(p), Ok(s)) = (tb.outer_position(), tb.outer_size()) {
            let r = (p.x, p.y, s.width as i32, s.height as i32);
            tb_rect = Some(r);
            if rects_adjacent(content, r) {
                rects.push(r);
            } else {
                warn_10s(&format!(
                    "[livepad] 工具条未贴合内容窗 rect={:?} vs {:?} → 本次不纳入光标活动范围",
                    r, content
                ));
            }
        }
    }

    let mut p = lock(pad());
    let step = dt(&mut p);

    let lxn = stick_norm(lx);
    let lyn = stick_norm(ly);
    let mag = (lxn * lxn + lyn * lyn).sqrt();
    let lt = lt8 > 30;
    let rt = rt8 > 30;
    let a = btn & BTN_A != 0;
    let b = btn & BTN_B != 0;
    let y = btn & BTN_Y != 0;

    /* ---- LT 组合：移窗优先于缩放 ---- */
    if lt && mag > DEAD {
        let dx = shaped(lxn) * BASE_SPEED * step * 0.7;
        let dy = -shaped(lyn) * BASE_SPEED * step * 0.7;
        drop(p);
        let _ = app.emit_to(
            "live-toolbar",
            "livepad://move",
            serde_json::json!({ "dx": dx, "dy": dy }),
        );
        p = lock(pad());
    } else if lt {
        // zoom 内部 emit_to 会阻塞等待主线程投递给 webview：必须先放掉 pad()，
        // 否则主线程若在 note_nav/set_mode 等 pad() 会与本线程互锁（AppHang）。
        drop(p);
        zoom(app, -1);
        p = lock(pad());
    } else if rt {
        drop(p);
        zoom(app, 1);
        p = lock(pad());
    } else {
        /* ---- 虚拟光标（CursorRouter + VirtualCursor） ---- */
        let mut cur = POINT::default();
        let real = (unsafe { GetCursorPos(&mut cur).is_ok() }).then(|| (cur.x, cur.y));
        if !p.dragging && !p.moved_this_tick {
            if let Some((cx, cy)) = real {
                if p.owner == CursorOwner::Controller
                    && (cx - p.last_set.0).abs() + (cy - p.last_set.1).abs() > 2
                {
                    p.owner = CursorOwner::Physical;
                }
            }
        }
        if mag > DEAD {
            if p.owner != CursorOwner::Controller {
                if let Some((cx, cy)) = real {
                    p.vx = cx as f64;
                    p.vy = cy as f64;
                }
                p.owner = CursorOwner::Controller;
            }
            let sens = sens_cached(app);
            p.vx += shaped(lxn) * BASE_SPEED * sens * step;
            p.vy += -shaped(lyn) * BASE_SPEED * sens * step;
            let (cx, cy) = clamp_to_region(p.vx, p.vy, &rects);
            p.vx = cx;
            p.vy = cy;
            let (tx, ty) = (p.vx.round() as i32, p.vy.round() as i32);
            p.moved_this_tick = (tx, ty) != p.last_set;
            if p.moved_this_tick {
                let dragging = p.dragging;
                let drag_win = p.a_target.clone();
                // 光标形状：每 50ms 问一次页面「这里该用什么指针」（输入框 I 形/按钮手型）
                let ask = p
                    .cursor_ask_at
                    .map(|t| t.elapsed() > Duration::from_millis(50))
                    .unwrap_or(true);
                let ask_win = if ask { click_target(app, POINT { x: tx, y: ty }, tb_rect) } else { None };
                if ask {
                    p.cursor_ask_at = Some(Instant::now());
                }
                // 拖动期间要把指针移动也发给页面（进度条/音量条/滑块跟着走）。
                // 节流 30ms：每次 eval 都要过 WebView2 IPC，16ms 一次没必要。
                let emit_move = dragging
                    && p.drag_eval_at
                        .map(|t| t.elapsed() > Duration::from_millis(30))
                        .unwrap_or(true);
                if emit_move {
                    p.drag_eval_at = Some(Instant::now());
                }
                drop(p);
                send_cursor(tx, ty);
                if emit_move {
                    if let Some(lb) = drag_win {
                        eval_in(app, &lb, &js_mouse("move", tx, ty));
                    }
                }
                if let Some(lb) = ask_win {
                    eval_in(app, &lb, &js_cursor_query(tx, ty));
                }
                // 刚问过就重申一次（窗口在收到鼠标移动时会把指针设回自己的默认值，
                // 我们的设定必须在它之后落地）
                apply_cursor(app, ask);
                p = lock(pad());
                p.last_set = (tx, ty);
            }
        } else {
            p.moved_this_tick = false;
        }

        /* ---- A = 鼠标左键（按住 + 摇杆 = 标准拖动） ---- */
        if a && !p.a_prev {
            p.dragging = true;
            p.drag_eval_at = Some(Instant::now());
            let (px, py) = p.cursor_now(real);
            let tgt = click_target(app, POINT { x: px, y: py }, tb_rect);
            p.a_target = tgt.clone();
            drop(p);
            // 诊断：把「前台是谁 / 光标压在我们窗口上没有 / 点给哪个窗口」写进日志。
            // 前台不在我们这儿时顺手抢一次——页面内点击不依赖它，但 hover 高亮、
            // 输入框光标、键盘回退（Space）还是要靠它。
            let fg = crate::overlay::is_foreground(&win);
            if !fg {
                let _ = crate::overlay::force_foreground(&win);
            }
            log::info!(
                "[livepad] A 按下 → 页面内左键 down @({},{}) 目标={:?} 前台={} 命中我们窗口={:?}",
                px,
                py,
                tgt,
                if fg { "YES" } else { "NO" },
                hit_is_ours(app, POINT { x: px, y: py })
            );
            if let Some(lb) = tgt {
                eval_in(app, &lb, &js_mouse("down", px, py));
            }
            p = lock(pad());
        } else if !a && p.a_prev {
            let (px, py) = p.cursor_now(real);
            let lb = p.a_target.clone();
            drop(p);
            log::info!("[livepad] A 松开 → 页面内左键 up @({},{}) 目标={:?}", px, py, lb);
            if let Some(lb) = lb {
                eval_in(app, &lb, &js_mouse("up", px, py));
            }
            p = lock(pad());
            p.dragging = false;
            p.a_target = None;
        }
    }
    p.a_prev = a;

    /* ---- 右摇杆：上下=滚动，左右=视频 ±10s ---- */
    let rxn = stick_norm(rx);
    let ryn = stick_norm(ry);
    if ryn.abs() > 1e-3 {
        p.scroll += -shaped(ryn) * 1200.0 * step;
        let mut notches = 0i32;
        while p.scroll.abs() >= 100.0 {
            let notch = if p.scroll > 0.0 { 100.0 } else { -100.0 };
            notches += if notch > 0.0 { 1 } else { -1 };
            p.scroll -= notch;
        }
        if notches != 0 {
            drop(p);
            wheel(notches * 120);
            p = lock(pad());
        }
    } else {
        p.scroll = 0.0;
    }
    if rxn.abs() > 0.75
        && p.seek_at.map(|t| t.elapsed() > Duration::from_millis(500)).unwrap_or(true)
    {
        p.seek_at = Some(Instant::now());
        let secs = if rxn > 0.0 { 10.0 } else { -10.0 };
        drop(p);
        eval(app, &seek_js(secs));
        p = lock(pad());
    }

    /* ---- Y = 播放/暂停 ---- */
    if y && !p.y_prev {
        log::info!("[livepad] Y 按下 → toggle_play");
        drop(p);
        toggle_play(app);
        p = lock(pad());
    }
    p.y_prev = y;

    /* ---- X 键 = 锁定/解锁 ---- */
    // 全屏（纯净模式）会隐藏工具条，导航栏上的锁按钮够不到；锁定后又是"游戏正常玩"
    // 的态，所以手柄需要一个不经过工具条的入口。实际动作由工具条页面执行
    // （它持有 locked 状态与 applyLock 全流程），这里只发事件——和 Win+Shift+L 同一条。
    let x = btn & BTN_X != 0;
    if x && !p.x_prev {
        drop(p);
        log::info!("[livepad] X 按下 → 切换直播锁定/解锁");
        let _ = app.emit("live://toggle-lock", serde_json::Value::Null);
        p = lock(pad());
    }
    p.x_prev = x;

    /* ---- B：短按返回链 / 长按语音 ---- */
    // 边沿判定全交给 b_poll（状态机独立于 Pad：切模式不会把「按了多久/是否已进语音」
    // 抹掉；抬起有去抖，单帧抖动不再截断录音）。这里只执行副作用，且凡是要等识别线程
    // 或主线程的调用都先放掉 pad()。
    let ev = b_poll(b);
    if !matches!(ev, BEvent::None) {
        drop(p);
        match ev {
            BEvent::Press => crate::voice::begin(app.clone()),
            BEvent::Promote => {
                log::info!("[livepad] B 长按 ≥500ms → 启动语音");
                crate::voice::promote(app);
            }
            BEvent::VoiceEnd => crate::voice::stop(app.clone()),
            BEvent::ShortTap => {
                // 短按：把刚才那 0.5s 录音丢掉（不识别、不亮 HUD），再走返回链
                log::info!("[livepad] B 短按 → 返回链");
                crate::voice::cancel();
                back_chain(app.clone());
            }
            BEvent::None => {}
        }
        p = lock(pad());
    }

    // 光标形状重申：注入移动不触发 WM_SETCURSOR（窗口不会自己重设形状），游戏/系统也
    // 可能把形状改回箭头，所以控制态里定频再按一次页面想要的形状（400ms，开销忽略）。
    let reapply = p
        .cursor_apply_at
        .map(|t| t.elapsed() > Duration::from_millis(400))
        .unwrap_or(true);
    if reapply {
        p.cursor_apply_at = Some(Instant::now());
        drop(p);
        apply_cursor(app, true);
    }
}

/// 连续缩放：发给工具条，限频 30ms。调用方必须已释放 pad()（内部 emit_to 阻塞主线程）。
fn zoom(app: &AppHandle, dir: i32) {
    struct ZT(Mutex<Option<Instant>>);
    static Z: std::sync::OnceLock<ZT> = std::sync::OnceLock::new();
    let zt = Z.get_or_init(|| ZT(Mutex::new(None)));
    {
        let mut g = lock(&zt.0);
        if g.map(|t| t.elapsed() < Duration::from_millis(30)).unwrap_or(false) {
            return;
        }
        *g = Some(Instant::now());
    }
    let _ = app.emit_to(
        "live-toolbar",
        "livepad://zoom",
        serde_json::json!({ "dir": dir, "step": 0.06 }),
    );
}

fn sens_cached(app: &AppHandle) -> f64 {
    let v = SENS.load(Ordering::Relaxed);
    if v != 0 {
        return v as f64 / 100.0;
    }
    let raw = std::fs::read_to_string(
        app.path().app_data_dir().unwrap_or_default().join("config.json"),
    )
    .ok()
    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    .and_then(|v| {
        v.get("settings")?
            .get("livepadSens")?
            .as_f64()
            .map(|f| (f.clamp(0.2, 5.0) * 100.0) as u32)
    })
    .unwrap_or(100);
    SENS.store(raw, Ordering::Relaxed);
    raw as f64 / 100.0
}

/* ---------------- 锁定态 ---------------- */

fn locked_tick(app: &AppHandle, btn: u16) {
    let mut p = lock(pad());
    let _ = dt(&mut p);
    let b = btn & BTN_B != 0;

    // 拦截桥：en=2（真实状态抹 B）；inject_until 窗口内 inj=1（游戏看到 B 按下）。
    // 这里已持 pad()，绝不能调 gp_apply()——它的 MODE_LOCKED 分支会重入 lock(pad())，
    // std::Mutex 不可重入 → 轮询线程一进锁定态就自我死锁并永久持有 pad()，
    // 主线程随后的 note_nav / set_mode 拿不到 pad() 即卡死（表现为 AppHang 崩溃）。
    // 直接读已持有的 p.inject_until 并落底层 gp_set，避免重入。
    let inject = p.inject_until.map(|t| Instant::now() < t).unwrap_or(false);
    crate::overlay_inject::gp_set(2, inject as u32);

    // B 边沿判定同控制态（b_poll 独立于 Pad，不会被模式重置抹掉）。锁定态的区别：
    // 短按要把 B 回注给游戏（inject_until），长按走语音。
    match b_poll(b) {
        BEvent::None => {}
        BEvent::Press => {
            drop(p);
            crate::voice::begin(app.clone());
        }
        BEvent::Promote => {
            drop(p);
            crate::voice::promote(app);
        }
        BEvent::VoiceEnd => {
            drop(p);
            gp_apply();
            crate::voice::stop(app.clone());
            return;
        }
        BEvent::ShortTap => {
            // 短按：丢弃这段录音，B 照常回注给游戏（见上面 inject_until）
            p.inject_until = Some(Instant::now() + B_TAP_INJECT);
            drop(p);
            gp_apply();
            crate::voice::cancel();
            return;
        }
    }
}

/* ============================== 输入注入原语 ============================== */

fn send_cursor(x: i32, y: i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2) - 1;
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2) - 1;
        let ax = (((x - vx) as f64 / vw as f64) * 65535.0).round().clamp(0.0, 65535.0) as i32;
        let ay = (((y - vy) as f64 / vh as f64) * 65535.0).round().clamp(0.0, 65535.0) as i32;
        let inp = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: ax,
                    dy: ay,
                    mouseData: 0,
                    // MOUSEEVENTF_VIRTUALDESK：让 ABSOLUTE 的 0-65535 映射到整个虚拟桌面。
                    // 缺它时 Windows 只按主显示器归一化，多显示器（尤其竖排副屏）下
                    // 光标会落错位置 → hover 不亮、点击点偏（本次 bug 根因）。
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: usize::MAX,
                },
            },
        };
        let n = SendInput(&[inp], std::mem::size_of::<INPUT>() as i32);
        let mut cur = POINT::default();
        let _ = GetCursorPos(&mut cur);
        log::info!("[livepad] send_cursor 目标=({x},{y}) SendInput={n} 实际=({},{})", cur.x, cur.y);
    }
}

/// A 键 = 鼠标左键：**在页面里派发事件**，不再用 SendInput 注入。
///
/// 为什么（实测结论，别再改回 SendInput 点击）：
/// - 光标移动用 SendInput 是好的（能拿到真实的 :hover 高亮），滚轮同理；
/// - 但**按键**消息在下面这些场景会被 Windows/WebView2 吞掉：窗口不是活动窗口
///   （注入的左键被当成「激活点击」）、游戏持鼠标捕获（消息全进游戏，还会被注入
///   DLL 抹成 WM_NULL）。日志里能看到的症状是 `SendInput=1`（系统收下了）但页面
///   收不到 mousedown —— 表现就是「光标能动、A 键点了没反应、拖动不动」。
///   把窗口强抢到前台能改善，但游戏一抢回去就又失效（日志里频繁出现重激活）。
/// - 页面内派发与焦点/捕获/Z 序全都无关，且和 Y/B/语音/快进退走的是同一条
///   已验证可用的通道（WebView2 的 Tauri IPC）。
///
/// 入参是**物理屏幕坐标**，页面自己换算成 clientX/clientY：window.screenX/Y 是
/// CSS 像素、devicePixelRatio 是设备像素比，两者都在页面里，换算最稳（省得宿主
/// 猜 DPI/窗口原点）。
fn js_mouse(kind: &str, sx: i32, sy: i32) -> String {
    // kind: down / move / up；buttons 表示按下状态（拖动期间 move 也要带 1）
    let (body, buttons, pressure) = match kind {
        "down" => (
            r#"el.dispatchEvent(mk('pointerdown'));el.dispatchEvent(mk('mousedown'));
try{const f=(el.closest&&el.closest('input,textarea,select,button,a,[contenteditable=""],[contenteditable="true"],[tabindex]'))||el;f.focus&&f.focus({preventScroll:true});}catch(e){}"#,
            1,
            "0.5",
        ),
        "move" => (
            r#"try{el.dispatchEvent(mk('pointermove'));el.dispatchEvent(mk('mousemove'));}catch(e){}"#,
            1,
            "0.5",
        ),
        _ => (
            // 实测（headless Chromium）：手动派发的 click 事件在 Chromium 里**会**
            // 照常跑浏览器默认行为（链接跳转 / 勾选框切换都验过），所以这里只派发、
            // 绝不能再补一次原生 .click()——那会变成双击（勾选框切两次=没变）。
            r#"el.dispatchEvent(mk('pointerup',{buttons:0,pressure:0}));el.dispatchEvent(mk('mouseup',{buttons:0,pressure:0}));el.dispatchEvent(mk('click',{buttons:0,pressure:0,detail:1}));"#,
            0,
            "0",
        ),
    };
    format!(
        r#"(()=>{{try{{
let doc=document,px={sx}/(window.devicePixelRatio||1)-window.screenX,py={sy}/(window.devicePixelRatio||1)-window.screenY;
if(px<0||py<0||px>innerWidth||py>innerHeight)return 'outside';
let el=doc.elementFromPoint(px,py),guard=0;
// 同源 iframe：把坐标换算进内层文档继续定位（不少直播/消息页把内容放 iframe 里）。
// 跨域拿不到 contentDocument，只能返回 iframe-cross（这种内容暂时点不到）。
while(el&&el.tagName==='IFRAME'&&guard++<3){{try{{const d=el.contentDocument;if(!d)return 'iframe-cross';const r=el.getBoundingClientRect();px-=r.left;py-=r.top;doc=d;el=d.elementFromPoint(px,py);}}catch(e){{return 'iframe-cross';}}}}
if(!el)el=doc.documentElement;
const W=doc.defaultView||window,X=Math.round(px),Y=Math.round(py);
const base={{bubbles:true,cancelable:true,composed:true,view:W,clientX:X,clientY:Y,screenX:X,screenY:Y,button:0,buttons:{buttons},detail:1}};
const pe={{pointerId:1,pointerType:'mouse',isPrimary:true,width:1,height:1,pressure:{pressure}}};
const mk=(t,extra)=>new (t.indexOf('pointer')===0?W.PointerEvent:W.MouseEvent)(t,Object.assign({{}},base,pe,extra||{{}}));
{body}
return 'ok';
}}catch(e){{return 'err:'+e}}}})()"#
    )
}

/// A 键点击的目标窗口：光标落在工具条上就发给工具条（导航栏/地址栏/锁按钮都在
/// 那个窗口里，它不是内容页的一部分），否则发给内容窗。两个窗口用同一段脚本：
/// clientX/Y 由页面按屏幕坐标自算，工具条的 screenX 就是工具条窗口的位置。
fn click_target(app: &AppHandle, pt: POINT, tb: Option<(i32, i32, i32, i32)>) -> Option<String> {
    if let Some((tx, ty, tw, th)) = tb {
        if pt.x >= tx && pt.x <= tx + tw && pt.y >= ty && pt.y <= ty + th {
            if app.get_webview_window("live-toolbar").is_some() {
                return Some("live-toolbar".into());
            }
        }
    }
    app.get_webview_window("live-browser").map(|_| "live-browser".to_string())
}

/// 在指定窗口里执行页面脚本（live-browser / live-toolbar）
fn eval_in(app: &AppHandle, label: &str, js: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.eval(js);
    }
}

/* ============================== 光标形状（虚拟鼠标的指针反馈） ==============================
 * 问题：注入的 SetCursorPos/SendInput 移动**不会**让窗口重新设置光标形状——只有真实的
 * 鼠标移动才会触发 WM_SETCURSOR 那条链路，所以手柄把光标挪到输入框上时指针还是箭头，
 * 用户看不出"这里能输入"。
 * 做法：页面按 CSS `cursor` 报回关键字（输入框 text / 按钮 pointer / …），宿主在主线程
 * 用 SetCursor 把系统光标设成对应形状。SetCursor 只对调用线程自己的窗口生效，所以必须
 * 回主线程执行。 */
static CURSOR_KIND: AtomicU8 = AtomicU8::new(0);
static CURSOR_APPLIED: AtomicU8 = AtomicU8::new(9); // 9 = 尚未应用

/// CSS cursor 关键字 → 形状码（0 箭头 1 文本 2 手型 3 十字 4 等待）
fn map_cursor(css: &str) -> u8 {
    match css {
        "text" | "vertical-text" => 1,
        "pointer" => 2,
        "crosshair" => 3,
        "wait" | "progress" => 4,
        _ => 0,
    }
}

/// 问页面「光标下的元素要什么指针形状」，页面用事件回抛（不阻塞轮询线程）。
/// 注意 textarea/文本框的 computed cursor 通常是 `auto`（Chromium 内部才转成 I 形），
/// 所以这里按元素类型补一次判定，否则输入框上还是箭头。
fn js_cursor_query(sx: i32, sy: i32) -> String {
    format!(
        r#"(()=>{{try{{
const x=Math.round({sx}/(window.devicePixelRatio||1)-window.screenX),y=Math.round({sy}/(window.devicePixelRatio||1)-window.screenY);
let c='default';
if(x>=0&&y>=0&&x<=innerWidth&&y<=innerHeight){{
const el=document.elementFromPoint(x,y);
if(el){{
c=getComputedStyle(el).cursor||'default';
if(c==='auto'||c==='default'){{
const t=(el.tagName||'').toLowerCase();
const ty=(el.type||'').toLowerCase();
const editable=t==='textarea'||(t==='input'&&['button','checkbox','radio','submit','reset','file','image','color','range','hidden'].indexOf(ty)<0)||el.isContentEditable===true;
if(editable)c='text';
}}
}}
}}
window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{{event:'live://cursor',payload:c}});
}}catch(e){{}}}})()"#
    )
}

/// 把页面报回来的形状落到系统光标上（回主线程；force=true 时即使形状没变也重申一次）
fn apply_cursor(app: &AppHandle, force: bool) {
    let k = CURSOR_KIND.load(Ordering::Relaxed);
    if !force && CURSOR_APPLIED.swap(k, Ordering::Relaxed) == k {
        return;
    }
    CURSOR_APPLIED.store(k, Ordering::Relaxed);
    let _ = app.run_on_main_thread(move || unsafe {
        let id = match k {
            1 => IDC_IBEAM,
            2 => IDC_HAND,
            3 => IDC_CROSS,
            4 => IDC_WAIT,
            _ => IDC_ARROW,
        };
        if let Ok(h) = LoadCursorW(None, id) {
            SetCursor(Some(h));
        }
    });
}

/// 光标底下的顶层窗口是不是我们自己的（live-browser / live-toolbar）。
/// 用来诊断「点击到底落在谁身上」：None = 取不到句柄。
fn hit_is_ours(app: &AppHandle, pt: POINT) -> Option<bool> {
    let mut ours: Vec<isize> = Vec::new();
    for l in ["live-browser", "live-toolbar"] {
        if let Some(w) = app.get_webview_window(l) {
            if let Ok(h) = w.hwnd() {
                ours.push(h.0 as isize);
            }
        }
    }
    unsafe {
        let h = WindowFromPoint(pt);
        if h.is_invalid() {
            return None;
        }
        let root = GetAncestor(h, GA_ROOT);
        let r = if root.is_invalid() { h.0 as isize } else { root.0 as isize };
        Some(ours.contains(&r))
    }
}

fn wheel(delta: i32) {
    unsafe {
        let inp = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: usize::MAX,
                },
            },
        };
        let _ = SendInput(&[inp], std::mem::size_of::<INPUT>() as i32);
    }
}

fn send_space() {
    unsafe {
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_SPACE,
                    wScan: 0,
                    dwFlags: Default::default(),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_SPACE,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let _ = SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32);
    }
}

/* ============================== WebView2 桥 ============================== */

fn eval(app: &AppHandle, js: &str) {
    if let Some(w) = app.get_webview_window("live-browser") {
        let _ = w.eval(js);
    }
}

/// 页面探针结果通道：live-browser 页面脚本 invoke plugin:event|emit 回来
static PROBE_TX: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);

pub fn init(app: &AppHandle) {
    let _ = APP.set(app.clone());
    let _ = app.listen("live://pad-probe", move |e: tauri::Event| {
        let tx = lock(&PROBE_TX).take();
        if let Some(tx) = tx {
            let _ = tx.send(e.payload().to_string());
        }
    });
    // 页面回抛「光标下的 CSS cursor」→ 存形状码，由 tick 落到系统光标上
    let _ = app.listen("live://cursor", move |e: tauri::Event| {
        let raw = e.payload().trim_matches('"').to_string();
        let k = map_cursor(&raw);
        if CURSOR_KIND.swap(k, Ordering::Relaxed) != k {
            // 只在形状真的变了时记一条（拖动时指针会在元素间来回，量可控），
            // 排查「到了输入框还是箭头」时这行就是唯一线索
            let shown: String = raw.chars().take(24).collect();
            log::info!("[livepad] 光标形状 → {k}（页面 cursor={shown}）");
        }
    });
}

fn probe(app: &AppHandle, js: &str, timeout: Duration) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    *lock(&PROBE_TX) = Some(tx);
    eval(app, js);
    let r = rx.recv_timeout(timeout).ok();
    *lock(&PROBE_TX) = None;
    r
}

/* ---- 页面动作 JS ---- */

/// B 返回链第一步：正在输入→取消输入并回报 handled；全屏→退出并回报 handled；
/// 否则回报 none（宿主据此 history.back）。
const JS_BACK_PROBE: &str = r#"(()=>{try{
const a=document.activeElement;
if(a&&(a.isContentEditable||a.tagName==='INPUT'||a.tagName==='TEXTAREA')){a.blur();window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'live://pad-probe',payload:'handled'});return;}
if(document.fullscreenElement||document.webkitFullscreenElement){(document.exitFullscreen||document.webkitExitFullscreen).call(document);window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'live://pad-probe',payload:'handled'});return;}
window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'live://pad-probe',payload:'none'});
}catch(e){}})()"#;

fn back_chain(app: AppHandle) {
    std::thread::spawn(move || {
        let r = probe(&app, JS_BACK_PROBE, Duration::from_millis(150));
        if r.as_deref() == Some("handled") {
            return; // 页面已处理（取消输入 / 退出全屏）
        }
        let has_history = {
            let mut p = lock(pad());
            if p.nav_depth > 0 {
                p.nav_depth -= 1;
                p.back_pending = true;
                true
            } else {
                false
            }
        };
        if has_history {
            eval(&app, "try{history.back()}catch(e){}");
            let a2 = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(700));
                let still = {
                    let mut p = lock(pad());
                    if p.back_pending {
                        p.back_pending = false;
                        p.nav_depth += 1;
                        true
                    } else {
                        false
                    }
                };
                if still {
                    let _ = livepad_set_mode(a2, MODE_GAME);
                }
            });
        } else {
            let _ = livepad_set_mode(app, MODE_GAME);
        }
    });
}

fn toggle_play(app: &AppHandle) {
    const JS: &str = r#"(()=>{try{
const vs=[...document.querySelectorAll('video')].filter(v=>v.offsetWidth>0&&v.offsetHeight>0);
if(vs.length){vs.sort((a,b)=>b.offsetWidth*b.offsetHeight-a.offsetWidth*a.offsetHeight);const v=vs[0];v.paused?v.play().catch(()=>{}):v.pause();window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'live://pad-probe',payload:'handled'});return;}
}catch(e){}
window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'live://pad-probe',payload:'novideo'});})()"#;
    // probe 要等页面回包（最长 150ms）——绝不能在轮询线程里等，虚拟鼠标会卡一截
    let a = app.clone();
    std::thread::spawn(move || {
        let r = probe(&a, JS, Duration::from_millis(150));
        match r.as_deref() {
            Some("novideo") => {
                log::info!("[livepad] toggle_play：页面无可见视频 → 回退 Space");
                send_space();
            }
            None => log::warn!("[livepad] toggle_play：页面探针超时无回包（eval 未生效或页面无响应）"),
            Some(_) => log::info!("[livepad] toggle_play：已由页面 video 元素处理"),
        }
    });
}

/* ============================== 语音命令执行（voice.rs 回调） ============================== */

/// whisper.cpp 中文常输出繁体（如「暫停」），而命令关键词按简体写。这里对语音
/// 命令用到的字符做一张极小的繁→简映射（非全量转换，只覆盖本模块关键词），
/// 让「暫停/播放/快進/縮小/請輸入/中間/分鐘」等繁体回包也能命中。
fn to_simplified(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '暫' => '暂',
            '繼' => '继',
            '續' => '续',
            '進' => '进',
            '後' => '后',
            '縮' => '缩',
            '點' => '点',
            '調' => '调',
            '請' => '请',
            '輸' => '输',
            '間' => '间',
            '鐘' => '钟',
            '幫' => '帮',
            '個' => '个',
            '為' => '为',
            '於' => '于',
            other => other,
        })
        .collect()
}

/// 解析自然语言文本 → 结构化动作 → 执行。规则解析，不接 AI 直接执行通道。
pub fn voice_execute(app: &AppHandle, text: &str) {
    let text = &to_simplified(text.trim());
    let t = text.to_lowercase();
    if t.is_empty() {
        voice_done(app, false, "没听清，请重试");
        return;
    }
    if let Some(rest) = strip_input(text) {
        let ok = type_into_page(app, &rest);
        voice_done(
            app,
            ok,
            if ok { "已输入" } else { "没找到输入框，请先用左摇杆移动鼠标并按 A 点击输入框" },
        );
        return;
    }
    if contains_any(&t, &["暂停"]) {
        eval(app, JS_PAUSE);
        return voice_done(app, true, "已暂停");
    }
    if contains_any(&t, &["播放", "继续"]) {
        eval(app, JS_PLAY);
        return voice_done(app, true, "已播放");
    }
    if let Some(secs) = seek_secs(&t) {
        eval(app, &seek_js(secs));
        return voice_done(
            app,
            true,
            &format!("已{}{}秒", if secs > 0.0 { "快进" } else { "后退" }, secs.abs()),
        );
    }
    if contains_any(&t, &["放大", "大一点", "调大"]) {
        emit_zoom(app, 1, 0.25);
        return voice_done(app, true, "已放大");
    }
    if contains_any(&t, &["缩小", "小一点", "调小"]) {
        emit_zoom(app, -1, 0.25);
        return voice_done(app, true, "已缩小");
    }
    if contains_any(&t, &["退出全屏", "取消全屏"]) {
        eval(app, "try{(document.exitFullscreen||document.webkitExitFullscreen).call(document)}catch(e){}");
        return voice_done(app, true, "已退出全屏");
    }
    if contains_any(&t, &["全屏"]) {
        eval(app, JS_FULLSCREEN);
        return voice_done(app, true, "已全屏");
    }
    for (k, pos) in [
        ("右上角", "tr"),
        ("左上角", "tl"),
        ("右下角", "br"),
        ("左下角", "bl"),
        ("居中", "cc"),
        ("中间", "cc"),
    ] {
        if t.contains(k) {
            move_to_corner(app, pos);
            return voice_done(app, true, &format!("已移到{k}"));
        }
    }
    voice_done(app, false, "没听清，请重试");
}

fn voice_done(app: &AppHandle, ok: bool, msg: &str) {
    // 忙标记与阶段标记统一由 voice.rs 的 finish() 收尾（识别线程紧接着就会调）：
    // 这里再清一次会在「结果刚出、用户已按下一次 B」的窗口里把新会话的状态抹掉。
    let _ = app.emit(
        "voice://state",
        serde_json::json!({ "state": if ok { "success" } else { "error" }, "text": msg }),
    );
    let a = app.clone();
    let ms = msg.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(if ok { 1200 } else { 2000 }));
        let _ = a.emit("voice://state", serde_json::json!({ "state": "idle", "text": ms }));
    });
}

fn strip_input(raw: &str) -> Option<String> {
    for p in ["请输入", "帮我输入", "输入"] {
        if let Some(i) = raw.find(p) {
            let rest = raw[i + p.len()..].trim().to_string();
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

fn contains_any(t: &str, keys: &[&str]) -> bool {
    keys.iter().any(|k| t.contains(k))
}

fn seek_secs(t: &str) -> Option<f64> {
    let dir = if contains_any(t, &["快进", "前进"]) {
        1.0
    } else if contains_any(t, &["后退", "倒退", "回退"]) {
        -1.0
    } else {
        return None;
    };
    let n = parse_cn_number(t)?;
    let unit = if t.contains("分钟") || t.contains("分") { 60.0 } else { 1.0 };
    Some(dir * n * unit)
}

fn parse_cn_number(t: &str) -> Option<f64> {
    let mut num = String::new();
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else if !num.is_empty() {
            break;
        }
    }
    if !num.is_empty() {
        return num.parse().ok();
    }
    let map: &[(&str, f64)] = &[
        ("半分钟", 30.0),
        ("一分钟", 60.0),
        ("十秒", 10.0),
        ("一", 1.0),
        ("二", 2.0),
        ("两", 2.0),
        ("三", 3.0),
        ("四", 4.0),
        ("五", 5.0),
        ("六", 6.0),
        ("七", 7.0),
        ("八", 8.0),
        ("九", 9.0),
        ("十", 10.0),
    ];
    for (k, v) in map {
        if t.contains(k) {
            return Some(*v);
        }
    }
    None
}

fn seek_js(secs: f64) -> String {
    format!(
        r#"(()=>{{try{{const vs=[...document.querySelectorAll('video')].filter(v=>v.offsetWidth>0&&v.offsetHeight>0);if(!vs.length)return;vs.sort((a,b)=>b.offsetWidth*b.offsetHeight-a.offsetWidth*a.offsetHeight);const v=vs[0];v.currentTime=Math.max(0,v.currentTime+{secs});}}catch(e)}})()"#
    )
}

const JS_PAUSE: &str = r#"(()=>{try{const vs=[...document.querySelectorAll('video')].filter(v=>v.offsetWidth>0&&v.offsetHeight>0);if(vs.length){vs.sort((a,b)=>b.offsetWidth*b.offsetHeight-a.offsetWidth*a.offsetHeight);vs[0].pause();}}catch(e){}})()"#;
const JS_PLAY: &str = r#"(()=>{try{const vs=[...document.querySelectorAll('video')].filter(v=>v.offsetWidth>0&&v.offsetHeight>0);if(vs.length){vs.sort((a,b)=>b.offsetWidth*b.offsetHeight-a.offsetWidth*a.offsetHeight);vs[0].play().catch(()=>{});}}catch(e){}})()"#;
const JS_FULLSCREEN: &str = r#"(()=>{try{const vs=[...document.querySelectorAll('video')].filter(v=>v.offsetWidth>0&&v.offsetHeight>0);if(vs.length){vs.sort((a,b)=>b.offsetWidth*b.offsetHeight-a.offsetWidth*a.offsetHeight);const v=vs[0];v.requestFullscreen?v.requestFullscreen():v.webkitRequestFullscreen&&v.webkitRequestFullscreen();}}catch(e){}})()"#;

fn emit_zoom(app: &AppHandle, dir: i32, step: f64) {
    let _ = app.emit_to(
        "live-toolbar",
        "livepad://zoom",
        serde_json::json!({ "dir": dir, "step": step }),
    );
}

fn move_to_corner(app: &AppHandle, pos: &str) {
    let Some(win) = app.get_webview_window("live-browser") else { return };
    let Ok(size) = win.outer_size() else { return };
    let mon = match win.current_monitor() {
        Ok(Some(m)) => m,
        _ => return,
    };
    let mp = mon.position();
    let ms = mon.size();
    let pad = 16i32;
    let (x, y) = match pos {
        "tl" => (mp.x + pad, mp.y + pad),
        "tr" => (mp.x + ms.width as i32 - size.width as i32 - pad, mp.y + pad),
        "bl" => (mp.x + pad, mp.y + ms.height as i32 - size.height as i32 - pad),
        "cc" => (
            mp.x + (ms.width as i32 - size.width as i32) / 2,
            mp.y + (ms.height as i32 - size.height as i32) / 2,
        ),
        _ => (
            mp.x + ms.width as i32 - size.width as i32 - pad,
            mp.y + ms.height as i32 - size.height as i32 - pad,
        ),
    };
    let _ = app.emit_to(
        "live-toolbar",
        "livepad://move-to",
        serde_json::json!({ "x": x, "y": y }),
    );
}

fn type_into_page(app: &AppHandle, text: &str) -> bool {
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let js = format!(
        r#"(()=>{{try{{
const els=[...document.querySelectorAll('input,textarea,[contenteditable="true"],[contenteditable=""]')]
 .filter(e=>e.offsetWidth>0&&e.offsetHeight>0&&!e.disabled&&!['hidden','checkbox','radio','file'].includes((e.type||'').toLowerCase()));
if(!els.length){{window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{{event:'live://pad-probe',payload:'noinput'}});return;}}
const el=els[0];el.focus();const val={text_json};
if(el.isContentEditable){{document.execCommand('insertText',false,val);}}
else{{const d=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value')||Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype,'value');d&&d.set?d.set.call(el,val):el.value=val;el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));}}
window.__TAURI_INTERNALS__&&window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{{event:'live://pad-probe',payload:'ok'}});
}}catch(e){{}}}})()"#
    );
    matches!(probe(app, &js, Duration::from_millis(200)).as_deref(), Some("ok"))
}

/// 供 voice.rs 完成识别后回调
pub fn on_voice_text(text: String) {
    let Some(app) = app_handle() else { return };
    if text.trim().is_empty() {
        voice_done(app, false, "没听清，请重试");
    } else {
        voice_execute(app, &text);
    }
}
