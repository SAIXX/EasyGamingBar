//! WebView 存活看门狗：显卡驱动超时重置（TDR）或 WebView2 渲染进程崩溃后，
//! 窗口句柄还在、页面却是空白的（JS 永不执行）——表现为「点了没反应 / 界面不显示」，
//! 而且 Rust 侧没有任何报错，极难排查。
//!
//! 做法与直播页心跳同源：Rust 每 5s 广播 `app://ping`，各页面（shared/alive.ts）
//! 回 `app://pong {label}`；某个窗口超过 20s 没回应就判定 webview 已死，
//! 自动 reload 救回来（reload 有 30s 冷却，避免死循环狂刷）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Listener, Manager};

/// 纳入看护的窗口（常驻 / 预创建的隐藏窗）
const WATCHED: &[&str] = &[
    "bar",
    "settings",
    "widget",
    "game-intro",
    "quick-drawer",
    "tooltip",
    "fps-overlay",
    "live-toolbar",
];

const PING_EVERY: Duration = Duration::from_secs(5);
/// 超过这么久没收到 pong 就判定 webview 已死
const DEAD_AFTER: Duration = Duration::from_secs(20);
/// 同一窗口两次 reload 的最小间隔
const RELOAD_COOLDOWN: Duration = Duration::from_secs(30);
/// 进程刚启动的宽限：页面还在加载，别急着判定死亡
const BOOT_GRACE: Duration = Duration::from_secs(15);

fn pong_map() -> &'static Mutex<HashMap<String, Instant>> {
    static M: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn reload_map() -> &'static Mutex<HashMap<String, Instant>> {
    static M: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 每个窗口连续判定死亡的次数（reload 后仍然不回 → 升级处理）
fn streak_map() -> &'static Mutex<HashMap<String, u32>> {
    static M: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 悬浮条连续死亡到这个次数就重启整个进程：TDR 后 WebView2 控制器整个坏掉
/// （reload 报 0x8007139F），同进程内再也建不出可用的 webview。
const RESTART_AFTER: u32 = 2;
/// 进程重启冷却（避免反复重启）
const RESTART_COOLDOWN: Duration = Duration::from_secs(180);
/// 单个进程生命周期内最多重启次数
const RESTART_MAX: u32 = 5;

static LAST_RESTART: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static RESTARTS: OnceLock<Mutex<u32>> = OnceLock::new();

/// 重启进程：仅在悬浮条反复救不回来时触发（带冷却与次数上限）
fn maybe_restart(app: &AppHandle, now: Instant) {
    let slot = LAST_RESTART.get_or_init(|| Mutex::new(None));
    let count_slot = RESTARTS.get_or_init(|| Mutex::new(0));
    let mut n = count_slot.lock().unwrap_or_else(|e| e.into_inner());
    if *n >= RESTART_MAX {
        return;
    }
    if let Ok(last) = slot.lock() {
        if let Some(t) = *last {
            if now.duration_since(t) < RESTART_COOLDOWN {
                return;
            }
        }
    }
    *n += 1;
    if let Ok(mut last) = slot.lock() {
        *last = Some(now);
    }
    log::warn!(
        "看门狗：悬浮条 webview 反复无响应（第 {n} 次），重启 EasyGamingBar 以重建 WebView2 环境"
    );
    app.restart();
}

pub fn init(app: AppHandle) {
    let born = Instant::now();
    // 记录各窗口的上线时刻（pong）
    let h = app.clone();
    app.listen("app://pong", move |e| {
        let label = serde_json::from_str::<serde_json::Value>(e.payload())
            .ok()
            .and_then(|v| v.get("label").and_then(|l| l.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        if label.is_empty() {
            return;
        }
        if let Ok(mut m) = pong_map().lock() {
            m.insert(label, Instant::now());
        }
    });
    // ping 与判定放到独立线程，不占主线程
    thread::spawn(move || loop {
        thread::sleep(PING_EVERY);
        if born.elapsed() < BOOT_GRACE {
            let _ = h.emit("app://ping", ());
            continue;
        }
        let _ = h.emit("app://ping", ());
        let now = Instant::now();
        // 直接遍历已存在的窗口：组件窗口是 widget-<id>，逐一枚举才不会漏
        for (label, win) in h.webview_windows() {
            if !WATCHED.contains(&label.as_str()) && !label.starts_with("widget-") {
                continue;
            }
            // 隐藏窗口不参与判定：WebView2 会对隐藏页做渲染/定时器节流，
            // ping 收不到或不回属正常——此前预创建的隐藏窗（快捷抽屉/tooltip/
            // 设置中心）被每 30s 误判死亡反复重载（日志里抽屉页面挂载 106 次），
            // 面板/抽屉的使用中状态随之丢失（表现为「打开的界面莫名消失/失灵」）。
            // 隐藏期间真死了的窗口，重新显示后若仍不回 pong，20s 内照样会被救回。
            let should_be_alive =
                win.is_visible().unwrap_or(false) && !win.is_minimized().unwrap_or(false);
            if !should_be_alive {
                continue;
            }
            let last = pong_map()
                .lock()
                .ok()
                .and_then(|m| m.get(label.as_str()).copied());
            let dead = match last {
                None => true,
                Some(t) => now.duration_since(t) > DEAD_AFTER,
            };
            if !dead {
                continue;
            }
            // 冷却：连续判定死亡也不反复 reload
            let mut skip = false;
            if let Ok(m) = reload_map().lock() {
                if let Some(t) = m.get(label.as_str()) {
                    if now.duration_since(*t) < RELOAD_COOLDOWN {
                        skip = true;
                    }
                }
            }
            if skip {
                continue;
            }
            if let Ok(mut m) = reload_map().lock() {
                m.insert(label.to_string(), now);
            }
            let streak = streak_map()
                .lock()
                .ok()
                .map(|mut m| {
                    let v = m.get(label.as_str()).copied().unwrap_or(0) + 1;
                    m.insert(label.to_string(), v);
                    v
                })
                .unwrap_or(1);
            log::warn!(
                "看门狗：{label} 超过 {}s 无响应（webview 可能已死）→ 自动重载（第 {streak} 次）",
                DEAD_AFTER.as_secs()
            );
            if let Err(e) = win.reload() {
                log::warn!("看门狗：重载 {label} 失败：{e}");
            }
            // 重载仍然救不回来（TDR 后 WebView2 控制器已损坏）→ 重启整个进程
            if label == "bar" && streak >= RESTART_AFTER {
                maybe_restart(&h, now);
            }
        }
    });
}
