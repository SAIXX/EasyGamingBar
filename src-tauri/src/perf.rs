//! 性能监控：CPU / GPU / VRAM / RAM 占用采样 + 前台游戏检测 + 帧率转发。
//!
//! - 占用率：PDH 计数器（\Processor Information、\GPU Engine、\GPU Adapter Memory）+ GlobalMemoryStatusEx，
//!   由后台线程每 1s 采样一次，多窗口轮询共用同一份缓存。
//! - **帧率不自采**：fps / 1% Low 来自注入链路——`egb_hook.dll` 注入游戏后在
//!   Present detour 里用 QPC 量帧时间（OptiScaler 同款滑动窗口统计，含 1% Low），
//!   经共享内存回传，宿主侧 `overlay_inject::injected_fps()` 供数。帧率测的就是
//!   注入的那个游戏进程，不存在「监控目标跑偏」。未注入（开关关 / 反作弊拦截）
//!   时帧率没有值，界面显示 "--"。
//! - 游戏检测：前台窗口 + 覆盖整块显示器（全屏/无边框）判定；没铺满时按用户游戏库
//!   里的 exe 名补判（窗口化游戏）。

use serde_json::{json, Value};
use tauri::Manager;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_FMT, PDH_FMT_COUNTERVALUE,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_MORE_DATA, PDH_HCOUNTER,
    PDH_HQUERY,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetSystemTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetShellWindow,
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    GetWindowTextLengthW, IsWindowVisible,
};

/// FPS 悬浮窗窗口标识（与前端性能面板共用同一 label）
const OVERLAY_LABEL: &str = "fps-overlay";
/// 性能面板窗口（唯一另一个订阅 perf://status 的窗口）
const PERF_PANEL_LABEL: &str = "widget-performance";

/// 轻模式（UI 全部隐藏、游戏进行中）：跳过 PDH 占用率采集（GPU Engine 计数器
/// 在 AMD 上开销明显），帧率走注入链路不受影响
static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 粘性游戏会话：前台全屏检测在某些游戏/系统组合下会秒级抖动（检测 A→空→B→空…），
/// 直接消费会让悬浮窗的归属进程与帧率每秒切换（表现 = 帧数乱跳）。
/// 检测结果缺失时沿用上一个游戏最长 4s，真正切走后自然过期。
static STICKY_GAME: std::sync::Mutex<Option<(GameInfo, Instant)>> = std::sync::Mutex::new(None);
/// 游戏检测彻底消失后粘性条目的最长保留时间
const GAME_STICKY_SECS: u64 = 4;

/// 前台游戏检测（带粘性）。sync_overlay 与 snapshot_value 必须用同一份，
/// 否则归属与帧率口径会在同一秒内不一致。
/// `LAST_GAME_MISS` 记下最近一次判定失败的原因（供诊断日志），只在每秒一次的路径里写。
static LAST_GAME_MISS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
fn current_game_sticky() -> Option<GameInfo> {
    let mut st = lock(&STICKY_GAME);
    if let Some(g) = current_game_relaxed() {
        *st = Some((g.clone(), Instant::now()));
        return Some(g);
    }
    if let Some((g, t)) = st.as_ref() {
        if t.elapsed() < Duration::from_secs(GAME_STICKY_SECS) {
            return Some(g.clone());
        }
    }
    *st = None;
    None
}

/// 确保 perf 采样线程已启动（幂等）
fn ensure_sampler() {
    let mut sh = lock(shared());
    if sh.sampler_started {
        return;
    }
    sh.sampler_started = true;
    std::thread::Builder::new()
        .name("perf-sampler".into())
        .spawn(sampler_loop)
        .expect("spawn perf sampler");
}

pub fn set_light(on: bool) {
    LIGHT.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn light() -> bool {
    LIGHT.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Default)]
struct Sample {
    cpu: Option<f64>,
    gpu: Option<f64>,
    ram: Option<f64>,
    /// 已用/总量显存（MiB，多适配器求和）
    vram_used: Option<f64>,
    vram_total: Option<f64>,
}

#[derive(Default)]
struct Shared {
    sample: Sample,
    /// 采样线程守护：ensure_started 可能被反复触发（面板开关、轮询），线程只许起一次
    sampler_started: bool,
    /// 聚合/广播线程守护：同上
    aggregator_started: bool,
}

/// 广播快照用的应用句柄（setup 阶段注入）
static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// setup 阶段调用：注册每秒快照广播的事件源，并立即启动采样——
/// 任何窗口打开时数据已就绪，不必等首个面板触发
pub fn init(app: tauri::AppHandle) {
    // 开关打开时预创建 FPS 悬浮窗（隐藏），后续由 sync_overlay 按需显示；
    // 这样后台线程只做定位，不必在聚合线程里建窗。
    // **必须延后**：悬浮条窗口此刻也在建 webview，同帧并发建隐藏窗会让 WebView2
    // 报 0x80070578「无效的窗口句柄」——窗口句柄在、webview 是空的，之后每次
    // 定位都失败（failed to receive message from webview）。故错开 1.2s 再建。
    if crate::commands::fps_overlay_prefs(&app).0 {
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            let h2 = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                let _ = build_overlay(&h2);
            });
        });
    }
    let _ = APP.set(app);
    ensure_started();
}

fn shared() -> &'static std::sync::Mutex<Shared> {
    static S: OnceLock<std::sync::Mutex<Shared>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(Shared::default()))
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/* ============================== 对外命令 ============================== */

/// 一次完整快照：占用率缓存 + 前台游戏 + 该游戏的 FPS。
#[tauri::command]
pub async fn get_perf_status() -> Result<Value, String> {
    ensure_started();
    Ok(snapshot_value())
}

/// 组装当前快照（供命令与每秒广播共用）
fn snapshot_value() -> Value {
    // 粘性检测：检测抖动（A→空→B）不再让归属与帧率每秒切换
    let game = current_game_sticky();
    // 帧率唯一来源：注入链路——egb_hook.dll 在游戏进程内量帧时间（OptiScaler 同款）。
    // 注入目标就是前台游戏，天然不会测错进程；未注入成功（开关关 / 反作弊拦截 /
    // 游戏库外进程）就是没有值，界面如实显示 "--"，不做任何兜底。
    let (fps, low) = match game.as_ref() {
        Some(_) => {
            let v = crate::overlay_inject::injected_fps();
            (v.map(|(f, _)| f), v.map(|(_, l)| l))
        }
        // 没有前台游戏：帧率没有归属，一律不给数
        None => (None, None),
    };
    let (cpu, gpu, ram, vram_used, vram_total, vram_pct) = {
        let sh = lock(shared());
        let s = &sh.sample;
        let vram_pct = match (s.vram_used, s.vram_total) {
            (Some(u), Some(t)) if t > 0.0 => Some((u / t * 100.0).clamp(0.0, 100.0)),
            _ => None,
        };
        (s.cpu, s.gpu, s.ram, s.vram_used, s.vram_total, vram_pct)
    };

    // 每 10 秒一条「帧率归属」诊断：一眼区分是**没识别到游戏**还是**没注入成功**
    // ——「进游戏没数值」的两种根因，日志形态完全不同。
    if fps.is_some() {
        static LAST_DIAG: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
        let mut l = lock(&LAST_DIAG);
        let due = match *l {
            Some(t) => t.elapsed() >= Duration::from_secs(10),
            None => true,
        };
        if due {
            *l = Some(Instant::now());
            let miss = lock(&LAST_GAME_MISS).clone();
            match &game {
                Some(g) => log::info!(
                    "[fps] 已归属 {}(pid={}) 帧率={:?} 1%Low={:?}",
                    g.name,
                    g.pid,
                    fps,
                    low.map(|v| v.round() as i64)
                ),
                None => log::info!(
                    "[fps] 未识别到前台游戏：{}（帧率={:?}）",
                    miss.unwrap_or_else(|| "未知原因".into()),
                    fps
                ),
            }
        }
    }

    // ---- 显示值平滑（EMA）----
    // 注入链路每 1s 一跳：α=0.5 的滞后约一个采样周期，肉眼几乎无感，
    // 但能把「是否值得推送」的变化次数砍掉一大半（配合签名去重）。
    static SMOOTH: std::sync::Mutex<Option<f64>> = std::sync::Mutex::new(None);
    let fps = {
        let mut st = lock(&SMOOTH);
        let f = match (*st, fps) {
            (Some(p), Some(n)) => Some(p * 0.5 + n * 0.5),
            (_, n) => n, // 首次 / 断供恢复：直接用真值，不做滞后
        };
        *st = f;
        f.map(|v| v.round() as i64)
    };

    json!({
        "cpu": cpu.map(|v| (v * 10.0).round() / 10.0),
        "gpu": gpu.map(|v| (v * 10.0).round() / 10.0),
        "ram": ram.map(|v| (v * 10.0).round() / 10.0),
        "vram_used": vram_used.map(|v| v.round() as i64),
        "vram_total": vram_total.map(|v| v.round() as i64),
        "vram": vram_pct.map(|v| (v * 10.0).round() / 10.0),
        "game": game.map(|g| json!({
            "pid": g.pid,
            "name": g.name,
            "fps": fps,
            "fps_low": low.map(|v| v.round() as i64),
            "monitor": [g.mon_x, g.mon_y, g.mon_w, g.mon_h],
        })),
    })
}

/// 把 FPS 悬浮窗定位到游戏所在显示器（物理像素）的指定角落并显示。
/// pos：tl / tr / bl / br（左上、右上、左下、右下）。
#[tauri::command]
pub async fn place_fps_overlay(
    window: tauri::WebviewWindow,
    pos: String,
) -> Result<(), String> {
    // window 参数是调用方（性能面板），实际要定位的是 fps-overlay 窗口
    let overlay = window
        .get_webview_window(OVERLAY_LABEL)
        .ok_or("FPS 悬浮窗尚未创建")?;
    let game = current_game_sticky().ok_or_else(|| {
        log::warn!("place_fps_overlay: 未检测到前台全屏游戏");
        "未检测到前台全屏游戏".to_string()
    })?;
    place_overlay_at(&overlay, &game, &pos)
}

/* ============================== FPS 悬浮窗托管 ============================== */

/// 上次定位标记：pid + 显示器矩形；变化时才重新定位
static OVERLAY_KEY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// 开关 / 位置缓存（每 5s 重读一次配置，避免每秒解析配置文件）
static OVERLAY_PREFS: std::sync::Mutex<Option<(Instant, bool, String)>> =
    std::sync::Mutex::new(None);

/// 把悬浮窗摆到游戏所在显示器的指定角落并显示（pos：tl / tr / bl / br）。
fn place_overlay_at(win: &tauri::WebviewWindow, g: &GameInfo, pos: &str) -> Result<(), String> {
    let scale = win.scale_factor().map_err(|e| e.to_string())? as f64;
    let margin = (24.0f64 * scale).round() as i32;
    // 两行布局：主帧率 + 帧生成标注行
    let w = (200.0f64 * scale).round() as i32;
    let h = (96.0f64 * scale).round() as i32;
    // 先落到目标显示器（拿到正确 DPI），再定尺寸与角落
    win.set_position(tauri::PhysicalPosition::new(g.mon_x, g.mon_y))
        .map_err(|e| e.to_string())?;
    win.set_size(tauri::PhysicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    let (x, y) = match pos {
        "tr" => (g.mon_x + g.mon_w - w - margin, g.mon_y + margin),
        "bl" => (g.mon_x + margin, g.mon_y + g.mon_h - h - margin),
        "br" => (
            g.mon_x + g.mon_w - w - margin,
            g.mon_y + g.mon_h - h - margin,
        ),
        _ => (g.mon_x + margin, g.mon_y + margin),
    };
    win.set_position(tauri::PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    // 「显示桌面」/Win+D 会把悬浮窗最小化：show 前必须还原，否则停在最小化里（=消失）
    win.unminimize().map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    log::info!(
        "FPS 悬浮窗已定位: pos={pos} mon=({},{},{},{})",
        g.mon_x,
        g.mon_y,
        g.mon_w,
        g.mon_h
    );
    Ok(())
}

/// 创建悬浮窗窗口：无装饰、点击穿透、不抢焦点（属性与前端创建时保持一致）。
fn build_overlay(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    let win = tauri::WebviewWindowBuilder::new(
        app,
        OVERLAY_LABEL,
        tauri::WebviewUrl::App("fps-overlay.html".into()),
    )
    .title("FPS")
    .inner_size(200.0, 96.0)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .shadow(false)
    .focusable(false)
    .visible(false)
    .build()
    .ok()?;
    // 点击穿透：**只设 WS_EX_TRANSPARENT，绝不带 WS_EX_LAYERED**。
    // set_ignore_cursor_events 会带上 LAYERED，而全屏游戏走独立翻转时分层窗口会被
    // 踢出合成（画面消失只剩声音），LAYERED 自身也是一份额外的合成开销。
    // 透明背景由 tao 的 NOREDIRECTIONBITMAP 保证，去掉 LAYERED 不影响观感。
    let _ = crate::overlay::set_click_through(app.clone(), OVERLAY_LABEL.to_string(), true);
    Some(win)
}

/// 读取「开关 + 位置」，带 5s 缓存（面板里改动最迟 5s 生效）。
fn overlay_prefs(app: &tauri::AppHandle) -> (bool, String) {
    let mut cache = lock(&OVERLAY_PREFS);
    if let Some((t, enabled, pos)) = cache.as_ref() {
        if t.elapsed() < Duration::from_secs(5) {
            return (*enabled, pos.clone());
        }
    }
    let (enabled, pos) = crate::commands::fps_overlay_prefs(app);
    *cache = Some((Instant::now(), enabled, pos.clone()));
    (enabled, pos)
}

/// 由聚合线程每秒调用：悬浮窗生命周期只锚定「开关」——
/// 与前台游戏检测、UI 显隐彻底解耦（用户要求锁死：异环可以 2077 不可以，
/// 因为 2077 窗口化时检测不到全屏游戏；看视频时更没有游戏可言）。
/// 有游戏 → 跟随其显示器定位并显示注入链路测得的帧率；
/// 无游戏（窗口化游戏 / 看视频 / 桌面）→ 原地保持。
/// 唯一隐藏途径 = 关开关。
pub(crate) fn sync_overlay(app: &tauri::AppHandle) {
    let (enabled, pos) = overlay_prefs(app);
    if !enabled {
        if let Some(w) = app.get_webview_window(OVERLAY_LABEL) {
            // 已经隐藏就别再调，省掉每秒无谓的窗口操作
            if w.is_visible().unwrap_or(false) {
                let _ = w.hide();
            }
        }
        *lock(&OVERLAY_KEY) = None;
        return;
    }
    let game = current_game_sticky();
    let win = match app.get_webview_window(OVERLAY_LABEL) {
        Some(w) => w,
        None => {
            // 兜底：预创建没做成（如 webview 建窗失败）时请求主线程补建，
            // 不在聚合线程里直接建——非主线程建窗正是 0x80070578 的诱因之一
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                let _ = build_overlay(&handle);
            });
            return;
        }
    };
    // 最小化（Win+D / 快捷指令「显示桌面」）在 Win32 里 IsWindowVisible 仍为 true，
    // 必须一并判——否则显示桌面后悬浮窗永远停在最小化里（=消失）。
    let vis = win.is_visible().unwrap_or(false) && !win.is_minimized().unwrap_or(false);
    let mut last = lock(&OVERLAY_KEY);
    if let Some(g) = &game {
        let key = format!("{}:{}:{}:{}:{}", g.pid, g.mon_x, g.mon_y, g.mon_w, g.mon_h);
        if !vis || last.as_deref() != Some(key.as_str()) {
            if let Err(e) = place_overlay_at(&win, g, &pos) {
                log::warn!("FPS 悬浮窗定位失败: {e}");
                return;
            }
        }
        *last = Some(key);
        return;
    }
    // 无前台游戏：锁死不隐藏（窗口化游戏 / 看视频 / 桌面）。
    // 从未定位过 → 先落到当前所在显示器的指定角落；
    // 被最小化/隐藏过 → 还原显示，位置保持不动。
    if last.is_none() {
        if let Err(e) = place_overlay_default(&win, &pos) {
            log::warn!("FPS 悬浮窗默认定位失败: {e}");
        }
        *last = Some(String::new());
        return;
    }
    if !vis {
        let _ = win.unminimize();
        let _ = win.show();
    }
}

/// 无前台游戏时的兜底定位：悬浮窗当前所在显示器的指定角落
fn place_overlay_default(win: &tauri::WebviewWindow, pos: &str) -> Result<(), String> {
    let mon = win
        .current_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("无可用显示器")?;
    let mp = mon.position();
    let ms = mon.size();
    let g = GameInfo {
        pid: 0,
        name: String::new(),
        mon_x: mp.x,
        mon_y: mp.y,
        mon_w: ms.width as i32,
        mon_h: ms.height as i32,
    };
    place_overlay_at(win, &g, pos)
}

/// 应用退出时的清理：帧率来自注入链路，本模块没有需要停的采集会话，
/// 采样/聚合线程随进程退出自然结束。
pub fn shutdown() {}

/// panic 路径的兜底清理：同上，无内核会话需要停。
pub fn panic_shutdown() {}


/* ============================== 线程编排 ============================== */

/// 启动占用率采样线程与每秒聚合/广播线程（幂等常驻）。
/// 帧率由注入链路（egb_hook.dll + overlay_inject）供数，这里不拉任何帧率源。
fn ensure_started() {
    // 只在持锁期间改标记，出锁后再拉线程——ensure_sampler/ensure_aggregator
    // 内部会再抢同一把非重入锁，持锁调用 = 死锁（第二次起整个性能系统冻死）
    ensure_sampler();
    ensure_aggregator();
}

/// 上次发给 FPS 悬浮窗的（时刻, 签名）——用于「内容没变就不发」的去重
static LAST_OVERLAY_SENT: std::sync::Mutex<Option<(Instant, String)>> =
    std::sync::Mutex::new(None);
/// 上次发给性能面板的（时刻, 签名）
static LAST_PANEL_SENT: std::sync::Mutex<Option<(Instant, String)>> =
    std::sync::Mutex::new(None);

/// FPS 悬浮窗真正会画出来的那些字段 → 签名（**带量化死区**）。
///
/// 实测：帧数每秒都在抖（ETW 采样噪声 ±3、归属在「游戏 ↔ 未识别」之间反复），
/// 逐帧精确比对的话签名几乎每次都变，去重形同虚设（第一版实测省下 0 次）。
/// 所以改成粗粒度判定：帧数按 2 档、GPU 按 2%、显存按 64MiB。
/// 注意**只用于「要不要发」的决策**，实际显示的值仍是真实值——
/// 数字跳 ±1 用户根本看不见，却要搭上一次 IPC + 重绘，不划算。
fn overlay_signature(snap: &serde_json::Value) -> String {
    let g = snap.get("game");
    let i64of = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_i64()).unwrap_or(-1);
    let f64of = |v: Option<&serde_json::Value>| {
        v.and_then(|x| x.as_f64())
            .map(|f| f.round() as i64)
            .unwrap_or(-1)
    };
    // -1（无值）单独保留：有值 ↔ 无值 一定要发，否则界面会停在旧数字上
    let q = |v: i64, step: i64| if v < 0 { -1 } else { v / step };
    let fps = q(i64of(g.and_then(|v| v.get("fps"))), 2);
    // 1% Low 也在悬浮窗上显示，必须进签名，否则它变了却不推送（界面停在旧数字）
    let low = q(i64of(g.and_then(|v| v.get("fps_low"))), 2);
    let gpu = q(f64of(snap.get("gpu")), 2);
    let vu = q(i64of(snap.get("vram_used")), 64);
    let vt = q(i64of(snap.get("vram_total")), 64);
    format!("{fps}|{low}|{gpu}|{vu}|{vt}")
}

/// 性能面板比悬浮窗多显示 CPU / 内存 / 显存占比，单独一份签名
fn panel_signature(snap: &serde_json::Value) -> String {
    let f64of = |k: &str| {
        snap.get(k)
            .and_then(|x| x.as_f64())
            .map(|f| (f * 10.0).round() as i64)
            .unwrap_or(-1)
    };
    format!(
        "{}|{}|{}|{}",
        overlay_signature(snap),
        f64of("cpu"),
        f64of("ram"),
        f64of("vram")
    )
}

/// 一分钟统计一次「发了多少 / 省了多少」——优化效果的可观测凭据
fn note_emit(sent: bool) {
    static ACC: std::sync::Mutex<(u32, u32)> = std::sync::Mutex::new((0, 0));
    let mut a = lock(&ACC);
    if sent {
        a.0 += 1;
    } else {
        a.1 += 1;
    }
    if a.0 + a.1 >= 60 {
        log::info!(
            "[fps] 悬浮窗推送：近 60 秒实发 {} 次 / 去重省下 {} 次",
            a.0,
            a.1
        );
        a.0 = 0;
        a.1 = 0;
    }
}

/// 聚合/广播线程：每秒驱动悬浮窗定位与 perf://status 推送（帧率由注入链路提供）
fn ensure_aggregator() {
    let mut sh = lock(shared());
    if sh.aggregator_started {
        return;
    }
    sh.aggregator_started = true;
    drop(sh);
    std::thread::Builder::new()
        .name("fps-aggregator".into())
        .spawn(|| {
            let mut last_tick = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                let now = Instant::now();
                let secs = now.saturating_duration_since(last_tick).as_secs_f64();
                last_tick = now;
                if secs <= 0.0 { continue; }
                if let Some(app) = APP.get() {
                    // ---- 悬浮窗推送去重（掉帧优化）----
                    // 每秒 emit → IPC → JS → 渲染 → 重绘 是这条链路上最贵的固定开销：
                    // 一个 200×96 的小窗背后是整条 Chromium 渲染进程 + 合成器。
                    // 帧数稳定时（锁 60/144、桌面静置）显示内容根本没变，
                    // 直接不发——实测能砍掉绝大多数秒级推送。
                    // 5s 心跳兜底：防止签名不变但状态其实需要刷新（窗口重建等）。
                    // 游戏在前台 → 自动进轻模式：PDH 采样降频到 2s。
                    // GPU Engine 的通配符数组读取在部分驱动上是毫秒级开销，游戏满负荷时
                    // 会直接体现在帧时间上（此前 set_light 从未被调用，等于永远全速采样）。
                    set_light(current_game_sticky().is_some());
                    sync_overlay(app);
                    use tauri::Emitter;
                    let snap = snapshot_value();
                    // **定向发送，不用全局 emit**：emit 会给每一个 WebView
                    // （bar / settings / tooltip / game-intro / quick-drawer …）
                    // 都投递一次并执行 JS，游戏满负荷时这些无用开销就是掉帧与输入延迟。
                    // 真正订阅 perf://status 的只有 FPS 悬浮窗和性能面板。
                    let sig = overlay_signature(&snap);
                    let overlay_up = app
                        .get_webview_window(OVERLAY_LABEL)
                        .map(|w| w.is_visible().unwrap_or(false))
                        .unwrap_or(false);
                    {
                        let mut st = lock(&LAST_OVERLAY_SENT);
                        let due = match *st {
                            Some((t, ref s)) => *s != sig || t.elapsed() >= Duration::from_secs(5),
                            None => true,
                        };
                        if overlay_up && due {
                            let _ = app.emit_to(OVERLAY_LABEL, "perf://status", snap.clone());
                            *st = Some((Instant::now(), sig));
                            note_emit(true);
                        } else if overlay_up {
                            note_emit(false);
                        }
                        // 窗口不可见时不更新签名/时间：下次可见会立即补发一条
                    }
                    if let Some(w) = app.get_webview_window(PERF_PANEL_LABEL) {
                        if w.is_visible().unwrap_or(false) {
                            let psig = panel_signature(&snap);
                            let mut st = lock(&LAST_PANEL_SENT);
                            let due = match *st {
                                Some((t, ref s)) => {
                                    *s != psig || t.elapsed() >= Duration::from_secs(5)
                                }
                                None => true,
                            };
                            if due {
                                let _ = app.emit_to(PERF_PANEL_LABEL, "perf://status", snap);
                                *st = Some((Instant::now(), psig));
                            }
                        }
                    }
                }
            }
        })
        .expect("spawn fps aggregator");
}




/* ============================== 恢复：以下为截断丢失的核心函数 ============================== */

/// 前台进程排除：系统外壳等全屏但不属于游戏的东西
const PROCESS_EXCLUDE_EXTRA: &[&str] = &["applicationframehost", "msedgewebview2"];

/// 前台游戏信息
#[derive(Clone, Debug)]
pub struct GameInfo {
    pub pid: u32,
    pub name: String,
    pub mon_x: i32,
    pub mon_y: i32,
    pub mon_w: i32,
    pub mon_h: i32,
}

/// 按 PID 获取进程名（toolhelp32 快照）
/// 一次进程快照解析多个 pid 的进程名（去 .exe 后缀、小写）——
/// 快照成本随系统进程数线性增长，逐 pid 调用会在每秒广播里放大成可观 CPU
fn process_names_snapshot(pids: &[u32]) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    if pids.is_empty() {
        return out;
    }
    let wanted: std::collections::HashSet<u32> = pids.iter().copied().collect();
    let names = process_snapshot_names_all();
    for pid in pids {
        if let Some(n) = names.get(pid) {
            out.insert(*pid, n.clone());
        }
    }
    out
}

/// 全量进程表缓存（TTL 5s）。
/// Toolhelp32 全进程遍历在每秒路径里被调 3~5 次，系统 200+ 进程时每次都是几十~几百微秒；
/// 游戏满负荷时这几毫秒就是「不跟手」的一分子。进程名几乎不变，缓存掉完全够用。
static NAME_CACHE: std::sync::LazyLock<std::sync::Mutex<Option<(Instant, std::collections::HashMap<u32, String>)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));
const NAME_TTL_SECS: u64 = 5;

/// 全量进程表：pid → 进程名（去 .exe、小写）。带 5s 缓存，O(进程数) 只在过期时付一次。
fn process_snapshot_names_all() -> std::collections::HashMap<u32, String> {
    {
        let c = lock(&NAME_CACHE);
        if let Some((t, m)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(NAME_TTL_SECS) {
                return m.clone();
            }
        }
    }
    let m = snapshot_names_uncached();
    *lock(&NAME_CACHE) = Some((Instant::now(), m.clone()));
    m
}

/// 真正做一次 Toolhelp32 快照（只在缓存过期时调）
fn snapshot_names_uncached() -> std::collections::HashMap<u32, String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = std::collections::HashMap::new();
    let snap = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) => h,
        Err(_) => return out,
    };
    unsafe {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                let name = String::from_utf16_lossy(&entry.szExeFile);
                let name = name.trim_end_matches('\0');
                let name = name.trim_end_matches(".exe").to_lowercase();
                out.insert(pid, name);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// 按 PID 获取进程名（走 5s 缓存，不再每次都做全进程遍历）
fn process_name(pid: u32) -> Option<String> {
    process_snapshot_names_all().get(&pid).cloned()
}

/// 未缓存的单 pid 查询（保留原实现备用）
#[allow(dead_code)]
fn process_name_uncached(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snap, &mut entry) }.is_ok() {
        loop {
            if entry.th32ProcessID == pid {
                let name = String::from_utf16_lossy(&entry.szExeFile);
                let name = name.trim_end_matches('\0');
                // 去掉 .exe 后缀
                return Some(name.trim_end_matches(".exe").to_lowercase());
            }
            if unsafe { Process32NextW(snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe { let _ = CloseHandle(snap); }
    None
}

/// 检测前台游戏。
/// - `require_fullscreen = true`：必须铺满显示器才算（严格口径）。
/// - `false`：铺满算；**没铺满但前台进程在持续高帧率呈现**也算（窗口化游戏 / 视频）。
///
/// 判定失败时把原因写进 `LAST_GAME_MISS`（诊断用）。
fn current_game_ex(require_fullscreen: bool) -> Option<GameInfo> {
    unsafe {
        let mut visible = true;
        let mut miss = |why: String| -> Option<GameInfo> {
            *lock(&LAST_GAME_MISS) = Some(why);
            None
        };
        let mut hwnd = GetForegroundWindow();
        // 前台是本应用自己的窗口（玩家刚点过悬浮条 / 面板）时，游戏根本不可能是前台。
        // 回退到最近一次「非本进程」的前台窗口——否则只要 UI 一获得焦点，帧率就断供。
        if crate::overlay::own_window(hwnd) {
            if let Some(prev) = crate::overlay::last_foreign_foreground() {
                hwnd = prev;
            }
        }
        if hwnd.is_invalid() || hwnd == GetShellWindow() {
            return miss("前台是桌面外壳（无前台窗口）".into());
        }
        // 遮挡检测（UWP cloak）
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return miss("前台窗口被 DWM 遮挡（cloaked）".into());
        }
        if !IsWindowVisible(hwnd).as_bool() {
            // **不直接判死**：部分独占全屏游戏 / 引擎的前台窗口 IsWindowVisible 就是
            // FALSE（实测日志里连续出现「前台窗口不可见」而游戏明明在满帧跑）。
            // 记录状态，交给后面的 relaxed 判据：只要它在持续呈现就算。
            visible = false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return miss("GetWindowRect 失败".into());
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 {
            return miss("窗口矩形非法".into());
        }
        // 找到窗口所在显示器
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut m_info = MONITORINFO::default();
        m_info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut m_info).as_bool() == false {
            return None;
        }
        // 进程号与进程名（先取，后面的判定要用）
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return miss("取不到前台窗口进程号".into());
        }
        let name = match process_name(pid) {
            Some(n) => n,
            None => return miss(format!("进程名读取失败（pid={pid}）")),
        };
        if PROCESS_EXCLUDE_EXTRA.iter().any(|e| name.contains(e)) {
            return miss(format!("前台进程在排除列表：{name}"));
        }
        // 排除系统外壳
        let skip = ["explorer", "dwm", "textinputhost", "searchhost", "shellexperiencehost"];
        if skip.iter().any(|k| name.contains(k)) {
            return miss(format!("前台是系统外壳：{name}"));
        }
        // 覆盖面积：**不能要求四边严格相等**——独占全屏 / 无边框游戏常有 1~几像素偏差，
        // 甚至窗口被特意做得比显示器大一圈（躲任务栏、多显示器），严格相等会直接判成
        // 「没有游戏」→ 帧率无从归属 → 悬浮窗显示 "--"。按交集占显示器比例判定。
        let mw = m_info.rcMonitor.right - m_info.rcMonitor.left;
        let mh = m_info.rcMonitor.bottom - m_info.rcMonitor.top;
        let ix = rect.right.min(m_info.rcMonitor.right) - rect.left.max(m_info.rcMonitor.left);
        let iy = rect.bottom.min(m_info.rcMonitor.bottom) - rect.top.max(m_info.rcMonitor.top);
        let cover = if mw > 0 && mh > 0 {
            (ix.max(0) as f64 * iy.max(0) as f64) / (mw as f64 * mh as f64)
        } else {
            0.0
        };
        if cover < 0.95 || !visible {
            if require_fullscreen {
                return miss(format!(
                    "前台窗口不算铺满：窗口=({},{})-({},{}) 覆盖={:.1}% 可见={} 显示器=({},{})-({},{})",
                    rect.left, rect.top, rect.right, rect.bottom,
                    cover * 100.0, visible,
                    m_info.rcMonitor.left, m_info.rcMonitor.top,
                    m_info.rcMonitor.right, m_info.rcMonitor.bottom,
                ));
            }
            // 放宽口径：没铺满也没关系，只要它**是用户游戏库里的进程**就算（窗口化游戏）。
            // 原先靠「呈现速率 ≥30fps」判定，而帧率在注入链路上、窗口化进程通常没被注入，
            // 故改为按游戏库 exe 名匹配——比拿窗口尺寸瞎猜准，也不会把浏览器/播放器误判成游戏。
            let lower = name.to_ascii_lowercase();
            let stem = lower.strip_suffix(".exe").unwrap_or(lower.as_str());
            let hit = known_game_names()
                .iter()
                .any(|g| g == stem || stem.starts_with(g.as_str()));
            if !hit {
                return miss(format!(
                    "前台窗口覆盖仅 {:.1}%（可见={}），且 {}(pid={}) 不在游戏库：窗口化游戏需先加入游戏库才能识别",
                    cover * 100.0, visible, name, pid
                ));
            }
        }
        *lock(&LAST_GAME_MISS) = None;
        Some(GameInfo {
            pid,
            name,
            mon_x: m_info.rcMonitor.left,
            mon_y: m_info.rcMonitor.top,
            mon_w: m_info.rcMonitor.right - m_info.rcMonitor.left,
            mon_h: m_info.rcMonitor.bottom - m_info.rcMonitor.top,
        })
    }
}

/// 严格口径：只有铺满显示器的前台窗口才算游戏
fn current_game() -> Option<GameInfo> {
    current_game_ex(true)
}

/// 实际使用口径：铺满优先，否则看它是不是用户游戏库里的进程。
/// 对外可见：overlay_inject 要知道往哪个进程注入，必须与帧率归属同一份结果。
pub fn current_game_relaxed() -> Option<GameInfo> {
    current_game_ex(false)
}

/// 游戏库里的 exe 名（小写、去 .exe），供识别「这前台进程是不是用户认过的游戏」。
/// 每帧都读一次配置文件没必要，游戏库在会话中几乎不变。
static GAME_NAMES: std::sync::Mutex<Option<(Instant, Vec<String>)>> = std::sync::Mutex::new(None);
const GAME_NAMES_TTL_SECS: u64 = 30;

pub(crate) fn known_game_names() -> Vec<String> {
    let mut c = lock(&GAME_NAMES);
    if let Some((t, v)) = c.as_ref() {
        if t.elapsed() < Duration::from_secs(GAME_NAMES_TTL_SECS) {
            return v.clone();
        }
    }
    let v = match APP.get() {
        Some(app) => crate::commands::game_exe_names(app),
        None => Vec::new(),
    };
    *c = Some((Instant::now(), v.clone()));
    v
}

/// 读取通配符 PDH 计数器的全部实例值（PDH_MORE_DATA 时按系统给的大小扩容重试）。
/// 通配符路径必须走数组 API——PdhGetFormattedCounterValue 对它恒返回失败，
/// 这正是此前 GPU / VRAM 永远显示 "--" 的根因。
unsafe fn counter_array(counter: PDH_HCOUNTER, fmt: PDH_FMT) -> Option<Vec<f64>> {
    let mut size = 0u32;
    let mut count = 0u32;
    // 先探所需缓冲区大小（失败忽略，下面按估算值兜底）
    let _ = PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, None);
    for _ in 0..3 {
        let need = if size > 0 {
            size as usize
        } else {
            count as usize * std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() + 1024
        };
        let mut buf = vec![0u8; need];
        let mut sz = buf.len() as u32;
        let mut n = 0u32;
        let ret = PdhGetFormattedCounterArrayW(
            counter,
            fmt,
            &mut sz,
            &mut n,
            Some(buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
        );
        if ret == 0 {
            let items = std::slice::from_raw_parts(
                buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                n as usize,
            );
            let large = fmt.0 == PDH_FMT_LARGE.0;
            return Some(
                items
                    .iter()
                    .filter(|it| it.FmtValue.CStatus == 0) // PDH_CSTATUS_VALID_DATA
                    .map(|it| {
                        if large {
                            it.FmtValue.Anonymous.largeValue as f64
                        } else {
                            it.FmtValue.Anonymous.doubleValue
                        }
                    })
                    .collect(),
            );
        }
        if ret != PDH_MORE_DATA {
            return None;
        }
    }
    None
}

/// 显存总量：DXGI 各适配器 DedicatedVideoMemory 求和（字节）。启动时取一次即可。
fn vram_total_bytes() -> u64 {
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return 0;
        };
        let mut total = 0u64;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            if let Ok(desc) = adapter.GetDesc1() {
                total += desc.DedicatedVideoMemory as u64;
            }
            i += 1;
        }
        total
    }
}

/// PDH 占用率采样线程：每秒采集 CPU/GPU/VRAM/RAM。
/// GPU/VRAM 供性能面板与游戏内 FPS 悬浮窗共同使用，不能在轻模式整个跳过
/// （否则悬浮窗里 GPU/VRAM 永远 "--"）——轻模式只降频到 2s 控开销。
fn sampler_loop() {
    unsafe {
        let mut query: PDH_HQUERY = Default::default();
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return;
        }
        let mut cpu_counter: PDH_HCOUNTER = Default::default();
        let mut gpu_counter: PDH_HCOUNTER = Default::default();
        let mut vram_counter: PDH_HCOUNTER = Default::default();
        let _ = PdhAddEnglishCounterW(
            query,
            w!("\\Processor Information(_Total)\\% Processor Utility"),
            0,
            &mut cpu_counter,
        );
        // GPU 占用：所有进程 3D 引擎实例求和（任务管理器同口径）
        let _ = PdhAddEnglishCounterW(
            query,
            w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage"),
            0,
            &mut gpu_counter,
        );
        // VRAM 已用：各适配器专用内存 Dedicated Usage 求和（原始大整数，字节）
        let _ = PdhAddEnglishCounterW(
            query,
            w!("\\GPU Adapter Memory(*)\\Dedicated Usage"),
            0,
            &mut vram_counter,
        );
        let vram_total_mib = (vram_total_bytes() / (1024 * 1024)) as f64;

        let mut first = true;
        loop {
            let interval = if light() { 2000 } else { 1000 };
            if PdhCollectQueryData(query) != 0 {
                std::thread::sleep(Duration::from_millis(interval));
                continue;
            }
            if first {
                // 速率类计数器需要相邻两次采样才能出值
                PdhCollectQueryData(query);
                first = false;
            }

            let mut s = Sample::default();
            let mut fmt = PDH_FMT_COUNTERVALUE::default();
            if PdhGetFormattedCounterValue(cpu_counter, PDH_FMT_DOUBLE, None, &mut fmt) == 0 {
                s.cpu = Some(fmt.Anonymous.doubleValue.clamp(0.0, 100.0));
            }
            if let Some(vals) = counter_array(gpu_counter, PDH_FMT_DOUBLE) {
                let total: f64 = vals.into_iter().map(|v| v.clamp(0.0, 100.0)).sum();
                s.gpu = Some(total.clamp(0.0, 100.0));
            }
            if let Some(vals) = counter_array(vram_counter, PDH_FMT_LARGE) {
                let used: f64 = vals.into_iter().map(|v| v.max(0.0)).sum();
                s.vram_used = Some(used / (1024.0 * 1024.0)); // 字节 → MiB
            }
            if vram_total_mib > 0.0 {
                s.vram_total = Some(vram_total_mib);
            }
            let mut mem = MEMORYSTATUSEX::default();
            mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut mem).is_ok() && mem.ullTotalPhys > 0 {
                s.ram = Some(
                    (mem.ullTotalPhys - mem.ullAvailPhys) as f64 / mem.ullTotalPhys as f64 * 100.0,
                );
            }

            lock(shared()).sample = s;
            std::thread::sleep(Duration::from_millis(interval));
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 显存总量：DXGI 适配器求和，本机任何独显/核显都应 >1GiB
    #[test]
    fn vram_total_sane() {
        let total = vram_total_bytes();
        assert!(total > 1 << 30, "vram_total_bytes 异常: {total}");
    }

    /// GPU Engine 通配符数组读取：应返回非空实例集，且数值都在 [0,100]
    #[test]
    fn gpu_engine_array_reads() {
        unsafe {
            let mut query: PDH_HQUERY = Default::default();
            assert_eq!(PdhOpenQueryW(PCWSTR::null(), 0, &mut query), 0);
            let mut counter: PDH_HCOUNTER = Default::default();
            assert_eq!(
                PdhAddEnglishCounterW(
                    query,
                    w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage"),
                    0,
                    &mut counter
                ),
                0
            );
            assert_eq!(PdhCollectQueryData(query), 0);
            std::thread::sleep(Duration::from_millis(150));
            assert_eq!(PdhCollectQueryData(query), 0);
            let vals = counter_array(counter, PDH_FMT_DOUBLE).expect("数组读取失败");
            assert!(!vals.is_empty(), "GPU Engine 无实例");
            assert!(vals.iter().all(|v| (0.0..=100.0).contains(v)));
        }
    }
}
