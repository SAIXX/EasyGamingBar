//! 游戏内覆盖层（DLL 注入）——「整个悬浮条画进游戏画面」
//!
//! 为什么需要它：真·独占全屏（classic FSE）下桌面合成器被游戏接管，任何 topmost
//! 窗口都画不出来（Xbox Game Bar 也一样）。唯一的用户级解法是进到游戏进程里，
//! 在它 Present 之前把界面贴到交换链的后备缓冲上——这就是 RTSS / 微星小飞机做的事。
//!
//! 链路：
//!   [本进程] 悬浮条窗口 --PrintWindow--> DIB 位图 --UpdateSubresource--> 共享 D3D11 纹理
//!            └─ 句柄写入共享内存 Local\EGB_HOOK_<pid>
//!   [游戏进程] egb_hook.dll 在 Present 前把该纹理按 alpha 混合贴到后备缓冲
//!
//! 开关：settings.dllInject（默认关）。关闭后不再注入新进程，已注入的进程立即停止
//! 绘制（把 flags 的可见位清掉）。**不卸载 DLL**——虚表已被改写，强行 FreeLibrary
//! 会让游戏下一次 Present 跳到已释放的地址直接崩；只停止绘制是安全的。
//!
//! 共享内存布局必须与 hooks/egb_hook/src/lib.rs 的 Shared 逐字段一致。

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{Emitter, Manager};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_RESOURCE_MISC_SHARED,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SetWindowPos, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, VirtualAllocEx, FILE_MAP_WRITE, MEM_COMMIT, MEM_RESERVE,
    PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, OpenProcess, OpenProcessToken, WaitForSingleObject, PROCESS_CREATE_THREAD,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_OPERATION,
    PROCESS_VM_WRITE,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};

/* ============================== 共享内存布局（与 DLL 严格一致） ============================== */

const MAGIC: u32 = 0x4547_424F; // "EGBO"
const FLAG_VISIBLE: u32 = 1 << 0;
/// bit2：位图是**直通 alpha**（非预乘）。DLL 据此选混合方程，见 alpha_kind()。
const FLAG_STRAIGHT: u32 = 1 << 2;

#[repr(C)]
struct Shared {
    magic: u32,
    pid: u32,
    frames: u64,
    p8: u64,
    p22: u64,
    hooked: u32,
    status: u32,
    draws: u64,
    tex: u64,
    w: u32,
    h: u32,
    flags: u32,
    seq: u32,
    err: u32,
    x: i32,
    y: i32,
    mon_w: u32,
    mon_h: u32,
    /// 以下为诊断字段（blt/flip 哑交换链各改了几个槽位、D3D12 队列钩子、走了哪条绘制路径）
    hook_blt: u32,
    hook_flip: u32,
    hook_q: u32,
    path: u32,
    /// DLL 侧帧率统计（Present detour 内 QPC 帧时间滑动窗口，每秒回写）：
    /// 1% Low 与窗口平均 FPS 均为 ×100 定点，ft_cnt = 窗口内样本数
    low_x100: u32,
    avg_x100: u32,
    ft_cnt: u32,
    /* ---- 手柄输入拦截桥（直播浏览器手柄控制，见 egb_hook Shared::gp_* 注释）----
     * 宿主（live_pad.rs 经本模块）写 gp_en / gp_slots；DLL 写 gp_hooked / gp_ack。
     * 两侧结构体必须逐字段一致，改这里必须同步改 egb_hook/src/lib.rs。 */
    gp_hooked: u32, // 120 DLL 已改写的 XInput IAT 槽位数（0 = 拦截不可用）
    gp_en: u32,     // 124 0=直通 1=合成（控制态） 2=锁定（真实状态抹 B）
    gp_ack: u32,    // 128 DLL 确认到的 en（== gp_en 才稳定）
    gp_slots: [GpSlot; 4], // 132..212 每槽位的合成 XINPUT_STATE（en=1 时生效）
    gp_inj: u32,    // 212 en=2 时：非 0 = DLL 强制置位 B（短按回注点按窗口）
}

/// 一个 XInput 槽位的合成状态（与 egb_hook::GpSlot 逐字段镜像）
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GpSlot {
    pub result: u32, // 0=成功，1167=未连接
    pub packet: u32,
    pub buttons: u16,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

/// 绘制路径（Shared.path）
const PATH_D3D11: u32 = 1;
const PATH_D3D12: u32 = 2;

/* ============================== 状态 ============================== */

struct View {
    _file: HANDLE,
    ptr: *mut Shared,
}
// 视图指针与映射句柄只在注入线程里访问
unsafe impl Send for View {}

struct Gpu {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    tex: Option<ID3D11Texture2D>,
    handle: u64,
    w: u32,
    h: u32,
}

struct State {
    /// 当前已注入的进程
    pid: u32,
    name: String,
    view: Option<View>,
    gpu: Option<Gpu>,
    /// DLL 侧回写的状态（status / hooked / draws / frames）
    hooked: u32,
    status: u32,
    draws: u64,
    frames: u64,
    /// DLL 侧诊断：两条哑交换链各改写了几个槽位、D3D12 队列钩子、上次走的绘制路径
    hook_blt: u32,
    hook_flip: u32,
    hook_q: u32,
    path: u32,
    /// 人话说明，给设置中心显示
    note: String,
    /// 上次广播的签名，状态不变就不重复发事件
    last_publish: String,
    /// 上次记录的 alpha 采样签名（预乘? / 最小 / 最大），变了才打日志
    alpha_sig: String,
    /// 上次记录的钩子结果签名（blt/flip/队列），变了才打日志
    diag_sig: String,
    /// 上次发起注入的时刻（用于失败重试）
    inject_at: Option<Instant>,
    /// 对当前 pid 已发起的注入次数，封顶避免无限重试
    inject_tries: u32,
    /// 「检测不到游戏」的连续轮数：前台被别的窗口抢走会让检测瞬间变空，
    /// 立刻拆掉注入再重建只会疯狂 CreateRemoteThread，覆盖层也跟着一闪一闪
    missing: u32,
    /// 帧率测算基点：(上次 frames 计数, 上次时刻)，每秒由回读处刷新
    fps_prev: Option<(u64, Instant)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            pid: 0,
            name: String::new(),
            view: None,
            gpu: None,
            hooked: 0,
            status: 0,
            draws: 0,
            frames: 0,
            hook_blt: 0,
            hook_flip: 0,
            hook_q: 0,
            path: 0,
            note: String::new(),
            last_publish: String::new(),
            alpha_sig: String::new(),
            diag_sig: String::new(),
            inject_at: None,
            inject_tries: 0,
            missing: 0,
            fps_prev: None,
        }
    }
}

/// 只对游戏库里的进程注入：注入要改写目标进程的交换链虚表，属于「动别人内存」的动作，
/// 值得用「用户自己认过的游戏」这层过滤（检测口径是「前台铺满 95% 就算游戏」，误判过）。
fn is_known_game(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(lower.as_str());
    crate::perf::known_game_names()
        .iter()
        .any(|g| g == stem || stem.starts_with(g.as_str()))
}

/// 连续多少轮检测不到游戏才认定「游戏没了」。40ms 一轮 → 3s。
const MISSING_TICKS: u32 = 75;

/// 覆盖层绘制黑名单：这些游戏的渲染管线与我们的 D3D11On12 覆盖层实测不兼容
/// （CP2077：D3D12 + Agility SDK + 光追 + 异步计算，首帧画完后设备进异常态、
/// 帧率骤降后闪退。Starfield 同款症状：日志实测帧率先假高到 400+、冻结三个
/// 采样后骤降到 21，随后进程消失——D3D12 游戏挂 D3D11On12 覆盖层的通病）。
/// 命中则**照常注入**（保留 FPS 计帧与手柄 XInput 桥），
/// 但永不置可见位、不抓帧贴图——覆盖层静默关闭，游戏稳定运行。
fn overlay_draw_blacklisted(name: &str) -> bool {
    let stem = name.to_ascii_lowercase();
    const BLACKLIST: &[&str] = &[
        "cyberpunk2077",
        "cyberpunk2077.exe",
        "starfield",
        "starfield.exe",
    ];
    BLACKLIST.iter().any(|b| stem == *b || stem.starts_with(b.trim_end_matches(".exe")))
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/* ============================== 帧率输出 ==============================
 * DLL 在游戏进程内量帧时间（Present detour，OptiScaler 同款思路），这里每秒
 * 用 Δframes/Δt 算平均 FPS，并取回 DLL 统计的 1% Low。perf 模块把这里当作
 * 唯一帧率源（HWiNFO 链路已整体移除）。 */

/// (fps ×100, 1% low ×100)；无注入 / 游戏不在 = None。worker 每秒刷新。
static FPS_OUT: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// 注入链路测得的当前游戏帧率。只对注入成功的进程有数——
/// 天然满足「帧率归属 = 前台游戏」，不存在测错进程的可能。
pub fn injected_fps() -> Option<(f64, f64)> {
    let v = FPS_OUT.lock().unwrap_or_else(|e| e.into_inner());
    v.map(|(f, l)| (f as f64 / 100.0, l as f64 / 100.0))
}

fn fps_out_set(v: Option<(u32, u32)>) {
    *FPS_OUT.lock().unwrap_or_else(|e| e.into_inner()) = v;
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/* ============================== 手柄拦截桥（直播浏览器手柄控制） ==============================
 * live_pad.rs 经这里读写游戏进程内 egb_hook 的 gp_* 字段（见 Shared::gp_* 注释）。
 * en：0=直通 1=合成（控制态，游戏读到手柄静止） 2=锁定（真实状态抹 B）。
 * slots 只在 en=1 时被 DLL 读取；inj 只在 en=2 时生效（强制置位 B = 回注点按）。
 *
 * 关键：这几个函数**绝不能碰 state() 锁**。worker 线程每帧持 state() 期间会做
 * PrintWindow / SetWindowPos——它们跨线程等主线程泵消息。若主线程（livepad_set_mode
 * → gp_apply → gp_set）同时来抢 state()，就构成 AB-BA 死锁：主线程停止泵消息，
 * worker 的窗口调用永不返回，整个 UI 卡死（表现为 AppHang）。所以这里用一把独立的
 * 原子指针 + 极短临界区（GP_LOCK 内只读写共享内存，绝不做任何窗口/阻塞调用）。 */

/// 当前共享内存视图裸指针（0=无）。worker 建/拆视图时在 GP_LOCK 下更新。
static GP_PTR: AtomicUsize = AtomicUsize::new(0);

fn gp_lock() -> &'static Mutex<()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(()))
}

/// 写入拦截档位与回注标记（无注入视图时静默忽略——live_pad 进入模式前已校验可用性）
pub fn gp_set(en: u32, inj: u32) {
    let _g = lock(gp_lock());
    let p = GP_PTR.load(Ordering::Relaxed) as *mut Shared;
    if !p.is_null() {
        unsafe {
            (*p).gp_en = en;
            (*p).gp_inj = inj;
        }
    }
}

/// 写入 4 槽位合成状态（控制态进入时写一次全静止即可，无需持续刷新）
pub fn gp_write_slots(slots: &[GpSlot; 4]) {
    let _g = lock(gp_lock());
    let p = GP_PTR.load(Ordering::Relaxed) as *mut Shared;
    if !p.is_null() {
        unsafe {
            (*p).gp_slots = *slots;
        }
    }
}

/// 手柄拦截桥是否可用：已注入 + DLL 已改写至少一个 XInput 槽位。
/// 注意 DLL 侧 gp_sync 每秒才扫一次，注入刚成功时要等 1~2 秒才变 true。
pub fn gamepad_bridge_ready() -> bool {
    let _g = lock(gp_lock());
    let p = GP_PTR.load(Ordering::Relaxed) as *mut Shared;
    if p.is_null() {
        return false;
    }
    unsafe { (*p).gp_hooked > 0 }
}

/// 最近一次注入失败原因（空串＝没失败过）。
/// 给「手柄控制需要游戏内覆盖层」这类拒绝提示补上真实原因：否则用户只看到
/// 「请开启 DLL 注入 / 把当前游戏加入游戏库」，而真正的问题（例如游戏以管理员
/// 身份运行、本程序未提权）完全看不出来，只能瞎试。
static INJECT_ERR: OnceLock<Mutex<String>> = OnceLock::new();

fn set_inject_error(msg: &str) {
    let slot = INJECT_ERR.get_or_init(|| Mutex::new(String::new()));
    *lock(slot) = msg.to_string();
}

/// 最近一次注入失败原因（供 live_pad 的手柄控制拒绝提示引用）
pub fn inject_error() -> String {
    let slot = INJECT_ERR.get_or_init(|| Mutex::new(String::new()));
    lock(slot).clone()
}

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/* ============================== 覆盖层会话（呼出/收起） ==============================
 * 呼出界面时若注入已在当前游戏生效，走「覆盖层模式」：不显示任何真实窗口——
 * 任何 topmost 窗口都会打断独占全屏的独立翻转（画面闪/顿，Windows 合成规则，
 * 无法绕过）。悬浮条窗口被挪到虚拟桌面之外但保持可见：webview 继续渲染
 * （PrintWindow 照抓）、手柄路由照常；DLL 用记住的屏幕内矩形把界面画进游戏。 */

static UI_SESSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static UI_RECT: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, 0, 0));

pub fn ui_session_active() -> bool {
    UI_SESSION.load(std::sync::atomic::Ordering::Relaxed)
}

/// 屏幕内坐标（挪走前记住的），DLL 按它定位；同时是收回时的还原坐标
pub fn ui_rect() -> (i32, i32, i32, i32) {
    *lock(&UI_RECT)
}

pub fn begin_ui_session(x: i32, y: i32, w: i32, h: i32) {
    *lock(&UI_RECT) = (x, y, w, h);
    UI_SESSION.store(true, std::sync::atomic::Ordering::Relaxed);
    // 立刻唤醒 worker 抓第一帧 + 进入 1.5s 热相位（16ms 捕获）。
    // 不做这两件事时，呼出到画面出现要等最多 40ms 周期 + 游戏下一帧，
    // 用户体感就是「开 UI 卡顿、反应慢」（OptiScaler 进程内翻 bool 无此延迟）。
    HOT_UNTIL.store(now_ms() + HOT_MS, std::sync::atomic::Ordering::Relaxed);
    wake_worker();
    log::info!("[inject] 覆盖层会话开始：悬浮条画进游戏画面（真实窗口不显示）");
}

/// 结束会话；返回之前是否在会话中
pub fn end_ui_session() -> bool {
    let was = UI_SESSION.swap(false, std::sync::atomic::Ordering::Relaxed);
    if was {
        log::info!("[inject] 覆盖层会话结束");
    }
    was
}

/// 挪窗口到指定物理坐标（异步）。
///
/// 用 `SetWindowPos + SWP_ASYNCWINDOWPOS` 而不是 tao 的 `set_position`：后者内部
/// SendMessage 会**同步等目标窗口线程（=主线程）处理完**才返回。worker 线程每帧持
/// state() 锁时调它，一旦主线程同时在抢 state()（如点锁定按钮 → gp_set），就是
/// AB-BA 死锁。SWP_ASYNCWINDOWPOS 让请求排队后立即返回，调用方线程不再阻塞。
fn move_bar_to(bar_hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(
            bar_hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_ASYNCWINDOWPOS,
        );
    }
}

/// 覆盖层会话期间把悬浮条挪到虚拟桌面之外。**必须保持可见**：
/// worker 靠「窗口可见」决定要不要把画面推给 DLL，webview 也要继续渲染，
/// PrintWindow 才抓得到内容。而留在屏幕内的 topmost 窗口会打断游戏的独立翻转
/// （画面闪/顿，Windows 合成规则），所以只能挪走、不能隐藏。
pub fn park_bar_offscreen(app: &tauri::AppHandle) {
    if let Some(bar) = app.get_webview_window("bar") {
        if let Ok(raw) = bar.hwnd() {
            move_bar_to(HWND(raw.0), -32000, -32000);
        }
    }
}

/// 收起界面时立刻停画：游戏画面马上恢复干净，不用等 worker 下一轮发现窗口没可见。
/// 走 GP_PTR（不碰 state()）：本函数从主线程（hotkeys 快捷键回调）调用，若抢
/// state() 会与 worker 持锁做 PrintWindow 构成 AB-BA 死锁。
pub fn stop_overlay_now() {
    let _g = lock(gp_lock());
    let p = GP_PTR.load(Ordering::Relaxed) as *mut Shared;
    if !p.is_null() {
        unsafe { (*p).flags &= !FLAG_VISIBLE };
    }
}

/// 收起界面时结束会话 + 立刻停画；返回收起前是否处于会话中（决定要不要搬回窗口）
pub fn end_session_and_stop() -> bool {
    let was = end_ui_session();
    stop_overlay_now();
    was
}

/// 把会话期间被挪出屏幕的悬浮条搬回原来的屏幕内坐标。
/// 必须在窗口 hide 之后调用——先搬再隐会让真实窗口在游戏画面上闪一下。
pub fn restore_bar_position(app: &tauri::AppHandle) {
    if let Some(bar) = app.get_webview_window("bar") {
        let (x, y, _, _) = ui_rect();
        if let Ok(raw) = bar.hwnd() {
            move_bar_to(HWND(raw.0), x, y);
        }
        log::info!("[inject] 悬浮条已搬回屏幕内 ({x},{y})");
    }
}

/// 强制退出会话并把悬浮条窗口挪回屏幕内（开关关闭 / 游戏退出等兜底路径）
pub fn force_exit_session(app: &tauri::AppHandle) {
    if end_ui_session() {
        restore_bar_position(app);
    }
}

/// 覆盖层模式是否就绪：开关开 + 当前游戏已注入、已挂钩，**且 DLL 确实画出来过**。
/// 最后一条是关键：没画成功还进会话，用户会看不到任何界面（真窗口被隐藏、
/// 游戏里又没有图）。宁可回退真实窗口路径，也不能呼出个看不见的界面。
/// 全程走 GP_PTR/共享内存（DLL 自写 pid），不碰 state()——本函数从主线程调用。
pub fn overlay_mode_ready(app: &tauri::AppHandle) -> bool {
    if !crate::commands::dll_inject_enabled(app) {
        return false;
    }
    let Some(g) = crate::perf::current_game_relaxed() else {
        return false;
    };
    let _guard = lock(gp_lock());
    let p = GP_PTR.load(Ordering::Relaxed) as *mut Shared;
    if p.is_null() {
        return false;
    }
    // 黑名单游戏永不进覆盖层会话（不抓帧贴图）；悬浮条走真实置顶窗口路径。
    // 少了这道闸，游戏内若残留旧注入的 draws>0，会把悬浮条挪出屏幕又没人画，
    // 用户直接看不到界面。
    if overlay_draw_blacklisted(&g.name) {
        return false;
    }
    unsafe { (*p).pid == g.pid && (*p).hooked > 0 && (*p).draws > 0 }
}

/* ============================== DLL 解析与注入 ============================== */

static HOOK_DLL: OnceLock<std::path::PathBuf> = OnceLock::new();

fn resolve_hook_dll(app: &tauri::AppHandle) {
    let mut cands: Vec<std::path::PathBuf> = vec![];
    if let Ok(rd) = app.path().resource_dir() {
        cands.push(rd.join("hooks").join("egb_hook.dll"));
        // NSIS/MSI 安装后资源实际落在 <安装目录>\resources\hooks\（resource_dir
        // 返回的是安装目录本身），安装包版必须靠这条命中——实测漏了它导致
        // 「未找到 egb_hook.dll，覆盖层不可用」。
        cands.push(rd.join("resources").join("hooks").join("egb_hook.dll"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("resources").join("hooks").join("egb_hook.dll"));
            cands.push(dir.join("hooks").join("egb_hook.dll"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("resources").join("hooks").join("egb_hook.dll"));
        cands.push(cwd.join("src-tauri").join("resources").join("hooks").join("egb_hook.dll"));
    }
    for c in cands {
        if c.is_file() {
            log::info!("[inject] 使用注入 DLL：{}", c.display());
            let _ = HOOK_DLL.set(c);
            return;
        }
    }
    log::warn!("[inject] 未找到 egb_hook.dll，游戏内覆盖层不可用");
}

/// 目标进程是否以提权（管理员）身份运行。与 [`crate::autostart::is_elevated`] 的区别：
/// 那个查的是本进程自己，这个查游戏。
///
/// 为什么要查：游戏以管理员运行时，我们的进程（dev 版 asInvoker）申请
/// PROCESS_VM_WRITE / PROCESS_CREATE_THREAD 会被系统按完整性级别直接拒绝
/// （实测 Cyberpunk 2077：OpenProcess 返回错误 5，而 2077 与它的启动方 Steam
/// 都是 Elevated）。这种情况给出「请以管理员身份运行本程序」才是有用的提示，
/// 报「受保护进程」会让人往反作弊方向查错。
unsafe fn is_elevated_pid(pid: u32) -> bool {
    let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false;
    };
    let mut tok = HANDLE::default();
    if OpenProcessToken(p, TOKEN_QUERY, &mut tok).is_err() {
        let _ = CloseHandle(p);
        return false;
    }
    let mut elev = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = GetTokenInformation(
        tok,
        TokenElevation,
        Some(&mut elev as *mut TOKEN_ELEVATION as *mut c_void),
        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut len,
    )
    .is_ok();
    let _ = CloseHandle(tok);
    let _ = CloseHandle(p);
    ok && elev.TokenIsElevated != 0
}

/// OpenProcess → 远端写 DLL 路径 → CreateRemoteThread(LoadLibraryW)。
/// 失败时返回**可读原因**（写进设置页的诊断 note），不再只报「失败」。
unsafe fn inject(pid: u32, dll: &std::path::Path) -> Result<(), String> {
    use windows::core::PCSTR;
    let process = match OpenProcess(
        PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE,
        false,
        pid,
    ) {
        Ok(h) => h,
        Err(e) => {
            let code = e.code().0 as u32 & 0xFFFF;
            // 5 = ERROR_ACCESS_DENIED。游戏提权而本程序没提权是最常见的成因。
            let why = if code == 5 && is_elevated_pid(pid) {
                "游戏以管理员身份运行（如管理员启动的 Steam 会带起提权），本程序未提权".to_string()
            } else if code == 5 {
                "系统拒绝访问（反作弊/受保护进程，属预期）".to_string()
            } else {
                format!("系统拒绝打开进程（错误 {code}）")
            };
            log::warn!("[inject] OpenProcess({pid}) 失败：{why}");
            return Err(why);
        }
    };
    use std::os::windows::ffi::OsStrExt;
    let mut path: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    path.push(0);
    let bytes = path.len() * 2;
    let mem = VirtualAllocEx(
        process,
        None,
        bytes,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if mem.is_null() {
        let _ = CloseHandle(process);
        log::warn!("[inject] 远端内存分配失败");
        return Err("远端内存分配失败".into());
    }
    let _ = WriteProcessMemory(
        process,
        mem,
        path.as_ptr() as *const c_void,
        bytes,
        None,
    );
    let load_addr = GetModuleHandleW(windows::core::w!("kernel32.dll"))
        .ok()
        .and_then(|k| GetProcAddress(k, PCSTR(b"LoadLibraryW\0".as_ptr())));
    let Some(load) = load_addr else {
        let _ = CloseHandle(process);
        log::warn!("[inject] 找不到 LoadLibraryW");
        return Err("在本进程找不到 LoadLibraryW".into());
    };
    type LoadLibFn = unsafe extern "system" fn(*const u16) -> isize;
    let load: LoadLibFn = std::mem::transmute(load);
    let thread = CreateRemoteThread(
        process,
        None,
        0,
        Some(std::mem::transmute::<
            LoadLibFn,
            unsafe extern "system" fn(*mut c_void) -> u32,
        >(load)),
        Some(mem),
        0,
        None,
    );
    let out = match thread {
        Ok(t) => {
            let _ = WaitForSingleObject(t, 4000);
            let _ = CloseHandle(t);
            Ok(())
        }
        Err(e) => Err(format!(
            "CreateRemoteThread 失败（错误 {}）",
            e.code().0 as u32 & 0xFFFF
        )),
    };
    // 句柄不再用：旧实现到这里直接 return，每次注入（含 5 次重试）都漏一个进程句柄
    let _ = CloseHandle(process);
    if let Err(e) = &out {
        log::warn!("[inject] {e}");
    }
    out
}

/* ============================== 共享内存 ============================== */

unsafe fn open_view(pid: u32) -> Option<View> {
    let name: Vec<u16> = format!("Local\\EGB_HOOK_{pid}\0").encode_utf16().collect();
    let file = OpenFileMappingW(FILE_MAP_WRITE.0, false, PCWSTR(name.as_ptr())).ok()?;
    let view = MapViewOfFile(file, FILE_MAP_WRITE, 0, 0, 256);
    let ptr = view.Value as *mut Shared;
    if ptr.is_null() {
        return None;
    }
    if (*ptr).magic != MAGIC {
        log::warn!("[inject] 共享内存 magic 不匹配（0x{:x}）", (*ptr).magic);
        return None;
    }
    Some(View { _file: file, ptr })
}

unsafe fn close_view(v: Option<View>) {
    if let Some(v) = v {
        let addr = windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
            Value: v.ptr as *mut c_void,
        };
        let _ = windows::Win32::System::Memory::UnmapViewOfFile(addr);
        let _ = windows::Win32::Foundation::CloseHandle(v._file);
    }
}

/* ============================== D3D11 共享纹理 ============================== */

fn feature_levels() -> [D3D_FEATURE_LEVEL; 2] {
    [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]
}

unsafe fn ensure_gpu(g: &mut Option<Gpu>) -> bool {
    if g.is_some() {
        return true;
    }
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let levels = feature_levels();
    if D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        windows::Win32::Foundation::HMODULE::default(),
        D3D11_CREATE_DEVICE_FLAG(0),
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        Some(&mut ctx),
    )
    .is_err()
    {
        log::warn!("[inject] 创建 D3D11 设备失败，覆盖层不可用");
        return false;
    }
    let (Some(dev), Some(ctx)) = (dev, ctx) else {
        return false;
    };
    *g = Some(Gpu {
        dev,
        ctx,
        tex: None,
        handle: 0,
        w: 0,
        h: 0,
    });
    true
}

unsafe fn ensure_tex(g: &mut Gpu, w: u32, h: u32) -> bool {
    if g.tex.is_some() && g.w == w && g.h == h && g.handle != 0 {
        return true;
    }
    g.tex = None;
    g.handle = 0;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    if g.dev.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
        log::warn!("[inject] 创建共享纹理失败（{w}x{h}）");
        return false;
    }
    let Some(tex) = tex else { return false };
    let handle = match tex.cast::<IDXGIResource>() {
        Ok(res) => res.GetSharedHandle().ok(),
        Err(_) => None,
    };
    let Some(handle) = handle else {
        log::warn!("[inject] 取不到共享纹理句柄");
        return false;
    };
    g.handle = handle.0 as u64;
    g.tex = Some(tex);
    g.w = w;
    g.h = h;
    true
}

/* ============================== 悬浮条画面捕获 ============================== */

/// PrintWindow(PW_RENDERFULLCONTENT) 把窗口渲染进一张 top-down BGRA 的 DIB。
/// 悬浮条只有几十像素高，一次捕获的成本可以忽略；即使被游戏遮挡（FSE 下必然），
/// PW_RENDERFULLCONTENT 也会强制合成器重新出图。
unsafe fn capture_window(hwnd: HWND, w: i32, h: i32) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return None;
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biWidth: w,
        biHeight: -h, // 负值 = top-down，省一次翻转
        biPlanes: 1,
        biBitCount: 32,
        biCompression: windows::Win32::Graphics::Gdi::BI_RGB.0,
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbm.into());
        let _ = DeleteDC(hdc);
        return None;
    }
    let old = SelectObject(hdc, hbm.into());
    // PrintWindow 在 windows crate 里挂在 Win32_Storage_Xps 特性下，为它拉一整个
    // XPS 打印 API 不值得 —— user32.dll 里直接取就行，系统 Win10+ 一定有。
    let ok = match print_window_fn() {
        Some(f) => {
            // 第三参数 = PW_RENDERFULLCONTENT：强制合成器重新出图，被游戏遮挡也照抓
            let r = unsafe { f(hwnd, hdc, 0x0000_0002) };
            r != 0
        }
        None => false,
    };
    let mut out = None;
    if ok {
        let n = (w as usize) * (h as usize) * 4;
        out = Some(std::slice::from_raw_parts(bits as *const u8, n).to_vec());
    }
    if !old.is_invalid() {
        SelectObject(hdc, old);
    }
    let _ = DeleteObject(hbm.into());
    let _ = DeleteDC(hdc);
    out
}

type PrintWindowFn = unsafe extern "system" fn(HWND, HDC, u32) -> i32;

fn print_window_fn() -> Option<PrintWindowFn> {
    use windows::core::PCSTR;
    static CACHE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let cached = CACHE.load(std::sync::atomic::Ordering::Relaxed);
    if cached != 0 {
        return Some(unsafe { std::mem::transmute::<usize, PrintWindowFn>(cached) });
    }
    unsafe {
        let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::w!(
            "user32.dll"
        ))
        .ok()?;
        let p = windows::Win32::System::LibraryLoader::GetProcAddress(
            lib,
            PCSTR(b"PrintWindow\0".as_ptr()),
        )?;
        let addr = p as *const () as usize;
        CACHE.store(addr, std::sync::atomic::Ordering::Relaxed);
        Some(std::mem::transmute::<usize, PrintWindowFn>(addr))
    }
}

/// 判断抓下来的 BGRA 位图是「直通 alpha」还是「预乘 alpha」。
///
/// 为什么必须判：我们的混合方程在 DLL 里是写死的，选错就是两种截然不同的翻车——
///   · 数据是预乘却按直通混 → 边缘发灰、半透明处变亮（颜色被乘了两次 alpha）
///   · 数据是直通却按预乘混 → 半透明处直接过曝成白块
/// PrintWindow + PW_RENDERFULLCONTENT 出图到底是哪种语义，Windows 没承诺过，
/// 实测也随窗口是否分层而变，所以干脆在运行时自己测：
/// 预乘 alpha 下恒有 max(r,g,b) <= a，只要发现任何一个像素违反，就一定是直通 alpha。
///
/// 顺带回 alpha 的极值，用来诊断「抓到的图全透明（PrintWindow 没抓到内容）」。
fn alpha_kind(bits: &[u8]) -> (bool, u32, u32) {
    let mut straight = false;
    let mut mn: u32 = 255;
    let mut mx: u32 = 0;
    let n = bits.len() / 4;
    let mut i = 0usize;
    while i < n {
        let o = i * 4;
        let b = bits[o] as u32;
        let g = bits[o + 1] as u32;
        let r = bits[o + 2] as u32;
        let a = bits[o + 3] as u32;
        if !straight && r.max(g).max(b) > a {
            straight = true;
        }
        if a < mn {
            mn = a;
        }
        if a > mx {
            mx = a;
        }
        i += 3; // 隔几个像素采一个就够，别为这个多花时间
    }
    (straight, mn, mx)
}

/* ============================== 工作线程 ============================== */

/// 覆盖层常规刷新间隔：25fps。悬浮条内容变化不快，再高只是白烧 CPU。
const FRAME_MS: u64 = 40;
/// 呼出后的「热相位」捕获间隔：16ms。begin_ui_session 后的前 HOT_MS 里，
/// webview 正在出第一帧 + 展开动画进行中，25fps 会让用户觉得「反应慢」；
/// 悬浮条只有几十像素高，PrintWindow 成本可忽略，热相位用 60fps 换跟手。
const HOT_FRAME_MS: u64 = 16;
const HOT_MS: u64 = 1500;
/// 热相位截止点（UNIX 毫秒）。0=不在热相位。
static HOT_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 事件驱动唤醒：begin_ui_session 推这里，worker 的 wait_timeout 立刻返回，
/// 第一帧捕获不再等下一个 40ms 周期（呼出延迟从 ~80ms 降到 ~20ms）。
static WAKE: (std::sync::Mutex<bool>, std::sync::Condvar) =
    (std::sync::Mutex::new(false), std::sync::Condvar::new());

fn now_ms() -> u64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn wake_worker() {
    if let Ok(mut p) = WAKE.0.lock() {
        *p = true;
        WAKE.1.notify_one();
    }
}

/// 配置读取间隔（避免每帧读盘）
const CFG_MS: u64 = 1000;

pub fn init(app: tauri::AppHandle) {
    resolve_hook_dll(&app);
    let _ = APP.set(app.clone());
    std::thread::Builder::new()
        .name("overlay-inject".into())
        .spawn(move || worker(app))
        .expect("spawn overlay-inject");
}

fn worker(app: tauri::AppHandle) {
    let mut cfg_at = Instant::now() - Duration::from_secs(10);
    let mut enabled = false;
    // 会话期间「抓到全透明画面」的连续帧数（见下面的自愈逻辑）
    let mut blank_frames: u32 = 0;
    loop {
        // 热相位（刚呼出的 1.5s）用 16ms，平时 40ms；呼出事件会立刻唤醒，
        // 不用等满一个周期。
        let hot = HOT_UNTIL.load(std::sync::atomic::Ordering::Relaxed) > now_ms();
        let tick = if hot { HOT_FRAME_MS } else { FRAME_MS };
        {
            let p = match WAKE.0.lock() {
                Ok(p) => p,
                Err(e) => e.into_inner(),
            };
            let mut p = if *p {
                p
            } else {
                match WAKE.1.wait_timeout(p, Duration::from_millis(tick)) {
                    Ok(r) => r.0,
                    Err(poisoned) => poisoned.into_inner().0,
                }
            };
            *p = false;
        }
        if cfg_at.elapsed() >= Duration::from_millis(CFG_MS) {
            cfg_at = Instant::now();
            let next = crate::commands::dll_inject_enabled(&app);
            if next != enabled {
                enabled = next;
                log::info!("[inject] 游戏内覆盖层开关 → {}", if enabled { "开" } else { "关" });
                let mut st = lock(state());
                st.last_publish.clear();
            }
        }

        // 只对游戏库里的进程注入：注入会改写目标进程的交换链虚表，是「动别人内存」的
        // 动作。而检测口径是「前台窗口铺满 95% 就算游戏」，日志里真的出现过往 chrome /
        // wechat 注入（浏览器全屏 / 聊天窗口看图全屏都会被误判）。用「用户自己加进游戏库
        // 的进程」这一层过滤，代价只是没入库的游戏不会注入。
        let game = crate::perf::current_game_relaxed().filter(|g| is_known_game(&g.name));
        let mut st = lock(state());

        // 目标进程变了：换注入对象（旧的停止绘制，虚表已改，不卸载 DLL）
        let want = game.as_ref().map(|g| g.pid).unwrap_or(0);
        if !enabled {
            if st.pid != 0 {
                stop_drawing(&mut st);
                st.pid = 0;
                st.name.clear();
                st.note = "已关闭".into();
                publish(&app, &mut st);
            }
            fps_out_set(None);
            st.fps_prev = None;
            // 开关关闭：退出覆盖层会话并把窗口挪回屏幕内（否则它停在屏幕外）
            force_exit_session(&app);
            continue;
        }
        if want == 0 {
            fps_out_set(None);
            st.fps_prev = None;
            // 检测瞬间变空（前台被本应用/其它窗口抢走）不应立刻拆注入：那样会反复
            // CreateRemoteThread 重建，覆盖层也跟着一闪一闪。等它「空」够久才认。
            st.missing += 1;
            if st.pid != 0 && st.missing >= MISSING_TICKS {
                stop_drawing(&mut st);
                st.pid = 0;
                st.name.clear();
                st.note = "未检测到游戏".into();
                publish(&app, &mut st);
                // 游戏退出：会话已无绘制对象，窗口回屏幕内待命
                force_exit_session(&app);
            }
            continue;
        }
        st.missing = 0;
        if want != st.pid {
            // 先让上一个进程停手（否则它会一直画着悬浮条），再关掉它的映射
            stop_drawing(&mut st);
            st.pid = want;
            st.name = game.as_ref().map(|g| g.name.clone()).unwrap_or_default();
            st.gpu = None; // 换进程要重建纹理（句柄跨进程不保证可复用）
            st.view = None;
            st.note.clear();
            st.fps_prev = None; // 帧率基点随进程作废
            st.inject_tries = 1;
            st.inject_at = Some(Instant::now());
            if let Some(dll) = HOOK_DLL.get() {
                let dll = dll.clone();
                let pid = want;
                match unsafe { inject(pid, &dll) } {
                    Ok(()) => {
                        log::info!("[inject] 已向 {} (pid={pid}) 注入覆盖层 DLL", st.name);
                        st.note = "已注入，等待首帧".into();
                        set_inject_error("");
                    }
                    Err(e) => {
                        set_inject_error(&e);
                        st.note = format!("注入失败：{e}");
                    }
                }
            } else {
                st.note = "缺少 egb_hook.dll".into();
            }
            publish(&app, &mut st);
        }

        // 注入后 DLL 要等 300ms 才建共享内存，期间持续重试打开
        if st.view.is_none() {
            st.view = unsafe { open_view(st.pid) };
            // 视图就绪 → 把手柄桥指针挂出去（GP_LOCK 下，见 gp_set 注释）
            if let Some(v) = st.view.as_ref() {
                let _g = lock(gp_lock());
                GP_PTR.store(v.ptr as usize, Ordering::Relaxed);
            }
            if st.view.is_none() {
                // 一直打不开 = DLL 没起来（游戏刚启动时被拦、LoadLibrary 走空等）。
                // 隔 2.5s 重试一次，封顶 5 次：LoadLibraryW 是幂等的（引用计数 +1），
                // DLL 侧的虚表改写也有「已是 detour 就不再改」的自我保护。
                let due = st
                    .inject_at
                    .map(|t| t.elapsed() >= Duration::from_millis(2500))
                    .unwrap_or(true);
                if due && st.inject_tries < 5 {
                    st.inject_tries += 1;
                    st.inject_at = Some(Instant::now());
                    if let Some(dll) = HOOK_DLL.get() {
                        let dll = dll.clone();
                        let pid = st.pid;
                        let r = unsafe { inject(pid, &dll) };
                        let n = st.inject_tries;
                        match r {
                            Ok(()) => log::info!(
                                "[inject] 共享内存未就绪，重试注入 pid={pid}（第 {n} 次）→ 已发起"
                            ),
                            Err(e) => {
                                log::info!(
                                    "[inject] 共享内存未就绪，重试注入 pid={pid}（第 {n} 次）→ 失败：{e}"
                                );
                                set_inject_error(&e);
                                st.note = format!("注入失败：{e}");
                                publish(&app, &mut st);
                            }
                        }
                    }
                }
                continue;
            }
        }

        let Some(g) = game.as_ref() else { continue };
        let Some(v) = st.view.as_ref() else { continue };
        let p = v.ptr;

        // 回读 DLL 状态
        unsafe {
            st.hooked = (*p).hooked;
            st.status = (*p).status;
            st.draws = (*p).draws;
            st.frames = (*p).frames;
            st.hook_blt = (*p).hook_blt;
            st.hook_flip = (*p).hook_flip;
            st.hook_q = (*p).hook_q;
            st.path = (*p).path;
        }
        // ---- 帧率：Δframes/Δt 当平均 FPS，1% Low 用 DLL 侧滑动窗口统计 ----
        // 每秒一次（worker 本体 25fps，足够密）。DLL 未回写统计（low=0）时以
        // 注入刚就绪、窗口还没攒满解释，先只给平均帧率。
        {
            let due = st
                .fps_prev
                .as_ref()
                .map(|(_, t)| t.elapsed() >= Duration::from_secs(1))
                .unwrap_or(true);
            if due {
                let now = Instant::now();
                let low_x100 = unsafe { (*p).low_x100 };
                let (fps_x100, low_out) = match st.fps_prev {
                    Some((pf, t)) => {
                        let dt = t.elapsed().as_secs_f64();
                        if dt > 0.2 && st.frames >= pf {
                            let f = ((st.frames - pf) as f64 / dt * 100.0) as u32;
                            let l = if low_x100 > 0 { low_x100 } else { f };
                            (f.min(100_000), l.min(100_000))
                        } else {
                            (0, 0)
                        }
                    }
                    None => (0, 0),
                };
                if fps_x100 > 0 {
                    fps_out_set(Some((fps_x100, low_out)));
                }
                st.fps_prev = Some((st.frames, now));
            }
        }
        // 挂钩结果只打一次日志：排查「注入到底装上了没有」直接看这行。
        // blt/flip 各代表一张虚表（现代游戏走 flip），队列=0 时 D3D12 游戏画不出来。
        let diag = format!("{}:{}:{}", st.hook_blt, st.hook_flip, st.hook_q);
        if diag != st.diag_sig {
            st.diag_sig = diag;
            log::info!(
                "[inject] DLL 挂钩结果：blt={} 槽位 / flip={} 槽位 / D3D12 队列钩子={}",
                st.hook_blt,
                st.hook_flip,
                if st.hook_q > 0 { "装上了" } else { "没装上" }
            );
        }

        // 覆盖层绘制黑名单：命中则不抓帧、不置可见位（DLL 下一帧起就不贴图），
        // 但上方的 FPS 回读与手柄桥照常工作——保游戏不闪退，功能尽量不丢。
        if overlay_draw_blacklisted(&st.name) {
            unsafe { (*p).flags &= !FLAG_VISIBLE };
            if st.note != "该游戏已列入覆盖层黑名单（仅计帧）" {
                st.note = "该游戏已列入覆盖层黑名单（仅计帧）".into();
                log::info!("[inject] {}：覆盖层绘制已禁用（黑名单），仅保留帧率统计", st.name);
            }
            publish(&app, &mut st);
            continue;
        }
        // 悬浮条窗口：隐藏/最小化时不画
        let Some(bar) = app.get_webview_window("bar") else {
            unsafe { (*p).flags &= !FLAG_VISIBLE };
            continue;
        };
        let visible = bar.is_visible().unwrap_or(false) && !bar.is_minimized().unwrap_or(false);
        if !visible {
            unsafe { (*p).flags &= !FLAG_VISIBLE };
            st.note = "悬浮条已隐藏".into();
            publish(&app, &mut st);
            continue;
        }
        // 会话期间窗口必须在屏幕外，而前端展开/折叠或窗口位置兜底都可能把它挪回来。
        // 每帧确认一次（幂等、只挪一次），否则真实窗口又压回游戏画面上。
        if ui_session_active() {
            if let Ok(pos) = bar.outer_position() {
                if pos.x > -10_000 || pos.y > -10_000 {
                    park_bar_offscreen(&app);
                }
            }
        }
        let Ok(raw) = bar.hwnd() else { continue };
        let hwnd = HWND(raw.0);
        // 覆盖层会话期间窗口已被挪到屏幕外：用记住的屏幕内矩形定位与取尺寸
        let (x, y, w, h) = if UI_SESSION.load(std::sync::atomic::Ordering::Relaxed) {
            let r = ui_rect();
            if r.2 <= 0 || r.3 <= 0 {
                continue;
            }
            r
        } else {
            let mut rc = RECT::default();
            unsafe {
                if GetWindowRect(hwnd, &mut rc).is_err() {
                    continue;
                }
            }
            (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top)
        };
        if w <= 0 || h <= 0 {
            continue;
        }

        let gpu_ok = unsafe { ensure_gpu(&mut st.gpu) };
        if !gpu_ok {
            st.note = "D3D11 设备创建失败".into();
            publish(&app, &mut st);
            continue;
        }
        let gpu = st.gpu.as_mut().unwrap();
        let tex_ok = unsafe { ensure_tex(gpu, w as u32, h as u32) };
        if !tex_ok {
            st.note = "共享纹理创建失败".into();
            publish(&app, &mut st);
            continue;
        }
        let Some(bits) = (unsafe { capture_window(hwnd, w, h) }) else {
            st.note = "窗口画面捕获失败".into();
            publish(&app, &mut st);
            continue;
        };
        // alpha 语义每帧都测（窗口是否分层可能中途变化）；日志放在上传之后，
        // 免得跟 st.gpu 的可变借用打架
        let (straight, a_min, a_max) = alpha_kind(&bits);
        let sig = format!("{straight}:{a_min}:{a_max}");
        unsafe {
            let res: ID3D11Resource = gpu.tex.clone().unwrap().cast().unwrap();
            gpu.ctx.UpdateSubresource(
                &res,
                0,
                None,
                bits.as_ptr() as *const c_void,
                (w as u32) * 4,
                0,
            );
            // 坐标换算：窗口绝对坐标 → 相对显示器原点；DLL 再按显示器/后备缓冲比例缩放
            (*p).tex = gpu.handle;
            (*p).w = w as u32;
            (*p).h = h as u32;
            (*p).x = x - g.mon_x;
            (*p).y = y - g.mon_y;
            (*p).mon_w = g.mon_w as u32;
            (*p).mon_h = g.mon_h as u32;
            (*p).seq = (*p).seq.wrapping_add(1);
            if straight {
                (*p).flags |= FLAG_STRAIGHT;
            } else {
                (*p).flags &= !FLAG_STRAIGHT;
            }
            (*p).flags |= FLAG_VISIBLE;
        }
        // 会话期间悬浮条被挪到屏幕外：万一 PrintWindow 在屏幕外抓不到内容（全透明），
        // 界面就成了「真窗口已藏、游戏里又没图」的死局。发现就立刻结束会话、退回真实
        // 窗口路径——宁可画面动一下，也不能让用户呼出个看不见的界面。
        if ui_session_active() && a_max == 0 {
            blank_frames += 1;
            if blank_frames >= 25 {
                log::warn!(
                    "[inject] 挪到屏幕外后抓起画面全透明（PrintWindow 没内容）→ 结束会话，\
                     退回真实窗口路径"
                );
                force_exit_session(&app);
                blank_frames = 0;
                continue;
            }
        } else {
            blank_frames = 0;
        }
        if sig != st.alpha_sig {
            st.alpha_sig = sig;
            if a_max == 0 {
                log::warn!(
                    "[inject] 抓到的悬浮条画面全透明（alpha 恒 0）——PrintWindow 没拿到内容，\
                     覆盖层会是空的"
                );
            } else {
                log::info!(
                    "[inject] 悬浮条画面 {} alpha，范围 {a_min}~{a_max}",
                    if straight { "直通" } else { "预乘" }
                );
            }
        }
        st.note = note_for(st.status, st.hooked);
        publish(&app, &mut st);
    }
}

/// 停止当前进程绘制 + 关掉它的共享内存映射。
/// 顺序不能反：先清可见位，DLL 下一帧就不再贴纹理，之后卸载视图才安全。
fn stop_drawing(st: &mut State) {
    let old = st.view.take();
    // 先摘掉手柄桥指针（GP_LOCK 下），再解除拦截、关映射：
    // 顺序反了会让 gp_set 在 UnmapViewOfFile 之后仍拿着悬垂指针写
    {
        let _g = lock(gp_lock());
        GP_PTR.store(0, Ordering::Relaxed);
    }
    unsafe {
        if let Some(v) = &old {
            (*v.ptr).flags &= !FLAG_VISIBLE;
            // 拆映射前解除手柄拦截：游戏进程还活着（如目标丢失防抖期）时，
            // 不能让它的 XInput 永远停在合成/抹 B 档位
            (*v.ptr).gp_en = 0;
            (*v.ptr).gp_inj = 0;
        }
        close_view(old);
    }
    st.hooked = 0;
    st.status = 0;
    st.draws = 0;
    st.frames = 0;
    st.alpha_sig.clear();
    st.inject_at = None;
    st.inject_tries = 0;
}

/// DLL 状态码 → 人话
fn note_for(status: u32, hooked: u32) -> String {
    match status {
        1 => "已在游戏画面内绘制".into(),
        2 => format!("已挂钩（{hooked} 个槽位），但没找到可绘制的 D3D11/D3D12 交换链，只计帧"),
        3 => "已挂钩，共享纹理打不开".into(),
        4 => "绘制失败，已停止尝试".into(),
        5 => "缺少 d3dcompiler，无法建着色器".into(),
        6 => "已挂钩（D3D12），但还没抓到游戏的渲染队列".into(),
        0 => {
            if hooked > 0 {
                "已挂钩，等待首帧".into()
            } else {
                "DLL 已加载但未挂钩成功".into()
            }
        }
        _ => "未知状态".into(),
    }
}

/// 状态变化才广播，避免每帧给所有窗口发事件
fn publish(app: &tauri::AppHandle, st: &mut State) {
    let key = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}",
        st.pid, st.name, st.status, st.hooked, st.note, st.hook_blt, st.hook_flip, st.hook_q, st.path
    );
    if key == st.last_publish {
        return;
    }
    st.last_publish = key;
    let _ = app.emit("inject://status", status_value(st));
}

fn status_value(st: &State) -> Value {
    json!({
        "pid": st.pid,
        "name": st.name,
        "hooked": st.hooked,
        "status": st.status,
        "draws": st.draws,
        "frames": st.frames,
        "note": st.note,
        // 诊断：钩子分别装在哪张虚表上、D3D12 队列钩子、上次走的绘制路径
        "hookBlt": st.hook_blt,
        "hookFlip": st.hook_flip,
        "hookQueue": st.hook_q,
        "path": match st.path {
            PATH_D3D11 => "d3d11",
            PATH_D3D12 => "d3d12",
            _ => "none",
        },
    })
}

/// 供设置中心显示当前注入状态
#[tauri::command]
pub async fn get_inject_status() -> Result<Value, String> {
    let st = lock(state());
    Ok(status_value(&st))
}

pub fn shutdown() {}
