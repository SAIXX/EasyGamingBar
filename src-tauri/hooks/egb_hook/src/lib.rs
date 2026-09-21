//! EasyGamingBar 游戏内覆盖层 Hook（注入到游戏进程的 cdylib）
//!
//! 两件事：
//! 1. **帧计数**：改写 IDXGISwapChain::Present（虚表槽位 8）与
//!    IDXGISwapChain1::Present1（槽位 22），每次呈现累加计数写入共享内存。
//! 2. **游戏内绘制**：宿主进程把悬浮条画面渲染到一块跨进程共享的 D3D11 纹理上，
//!    本 DLL 在每次 Present 之前把它按 alpha 混合贴到游戏的后备缓冲上——
//!    这样在真·独占全屏（FSE，任何其他窗口都画不出来）下界面依然可见。
//!
//! 原理说明（改代码前务必读完）：
//! - 虚表改写靠「自建哑交换链 → 拿虚表 → 改槽位」。dxgi.dll 里 blt-model
//!   （CreateSwapChain）与 flip-model（CreateSwapChainForHwnd）是两套不同的
//!   实现、两张不同的虚表，**必须各建一条哑交换链各改一次**，只改一条的话
//!   现代游戏（绝大多数走 flip）一条 Present 都收不到——这正是旧版帧计数恒为 0
//!   的原因。
//! - 原函数地址按「虚表指针」归档：被改写后所有共享该虚表的交换链都会落到我们的
//!   detour，调用时用 this 的虚表指针回查原函数转发。
//! - 只在游戏设备是 **D3D11** 时才绘制。D3D12 / Vulkan 的后备缓冲我们打不开，
//!   直接放弃绘制（帧计数仍可用），绝不为了画覆盖层去动游戏的命令队列。
//! - 绘制前后完整保存/还原 OM / RS / IA / VS / PS 状态，尽量不给游戏留副作用；
//!   仍然有被反作弊识别的风险，所以功能在设置里默认关闭、由用户自行开启。
//!
//! 共享内存：Local\EGB_HOOK_<pid>，两侧（本 DLL 与宿主 overlay_inject.rs）
//! 必须严格按同一份 Shared 布局读写，改字段要同步改两边。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use windows::core::{s, w, BOOL, GUID, Interface, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11CreateDeviceAndSwapChain, ID3D11BlendState, ID3D11Buffer,
    ID3D11DepthStencilState, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
    ID3D11InputLayout, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_VERTEX_BUFFER,
    D3D11_BLEND, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD,
    D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_BLEND_SRC_ALPHA, D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT, D3D11_INPUT_ELEMENT_DESC,
    D3D11_INPUT_PER_VERTEX_DATA,
    D3D11_MAP_WRITE_DISCARD,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC,
    D3D11_VIEWPORT, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
};
use windows::Win32::Graphics::Direct3D11on12::{
    D3D11On12CreateDevice, D3D11_RESOURCE_FLAGS, ID3D11On12Device,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_RESOURCE_STATE_PRESENT, ID3D12CommandQueue,
    ID3D12Device, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
    DXGI_FORMAT_R32G32B32_FLOAT, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
    DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain, IDXGISwapChain1, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress, LoadLibraryW,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, VirtualProtect, FILE_MAP_WRITE, PAGE_PROTECTION_FLAGS,
    PAGE_READWRITE,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, HOOKPROC, MSG, SetWindowsHookExW, HC_ACTION, PM_REMOVE,
    WH_GETMESSAGE, WS_OVERLAPPED,
};

/* ============================== 共享内存布局 ============================== */

const MAGIC: u32 = 0x4547_424F; // "EGBO"

/// 宿主写 tex/w/h/flags/seq/x/y/mon_w/mon_h；本 DLL 写其余字段。
/// 宿主侧（overlay_inject.rs）有一份逐字段镜像，改这里必须同步改那里。
#[repr(C)]
struct Shared {
    magic: u32,          // 0
    pid: u32,            // 4
    frames: u64,         // 8   Present + Present1 合计
    p8: u64,             // 16  槽位 8 命中数
    p22: u64,            // 24  槽位 22 命中数
    hooked: u32,         // 32  成功改写的槽位数
    status: u32,         // 36  见 STATUS_*
    draws: u64,          // 40  已绘制帧数
    tex: u64,            // 48  宿主共享纹理句柄（KMT）
    w: u32,              // 56  覆盖层像素宽
    h: u32,              // 60  覆盖层像素高
    flags: u32,          // 64  bit0 显示 / bit1 宿主请求重建纹理
    seq: u32,            // 68  宿主每次更新纹理 +1
    err: u32,            // 72  最近一次 HRESULT
    x: i32,              // 76  覆盖层左上角（显示器坐标，相对显示器原点）
    y: i32,              // 80
    mon_w: u32,          // 84  显示器像素宽（用于把坐标换算到后备缓冲）
    mon_h: u32,          // 88
    hook_blt: u32,       // 92  blt 哑交换链改写的槽位数（诊断：0 = 这条钩子没装上）
    hook_flip: u32,      // 96  flip 哑交换链改写的槽位数（现代游戏全靠它）
    hook_q: u32,         // 100 D3D12 命令队列 ExecuteCommandLists 是否钩上
    path: u32,           // 104 上一次绘制走的路径：1=D3D11 直画 2=D3D12(D3D11On12)
    low_x100: u32,       // 108 1% Low × 100（帧率统计每秒回写一次）
    avg_x100: u32,       // 112 窗口平均 FPS × 100（诊断，宿主可用 Δframes 自算核对）
    ft_cnt: u32,         // 116 统计窗口内的样本帧数
    /* ---- 手柄输入拦截（直播浏览器手柄控制）----
     * XInput 是进程轮询模型、与 Windows 焦点无关，游戏即使不在前台也照样读手柄。
     * 唯一能在「浏览器控制态」让游戏收不到手柄的办法，就是在游戏进程内 hook
     * XInputGetState/XInputGetStateEx 的 IAT 入口。三档语义（gp_en）：
     *   0 = 直通：detour 原样转发原函数（对游戏零影响，默认）
     *   1 = 控制态：detour 不调原函数，返回宿主写进 gp_slots 的合成状态
     *       （控制态 = 静止手柄；宿主进入模式时写一次即可，无需持续刷新）
     *   2 = 锁定态：detour 调原函数拿真实状态后只抹掉 B 键位（零额外延迟，
     *       游戏照常玩）；gp_inj!=0 的窗口内反向强制置位 B——宿主短按 B 回注
     *       一颗干净点按：inj=1 时游戏看到 B 按下，80ms 后 inj=0 看到 B 抬起。
     * 宿主写 gp_en / gp_slots / gp_inj；本 DLL 写 gp_hooked / gp_ack。
     * 宿主侧（overlay_inject.rs）有一份逐字段镜像，改这里必须同步改那里。 */
    gp_hooked: u32,      // 120 已改写的 XInput IAT 槽位数（0 = 拦截不可用）
    gp_en: u32,          // 124 宿主请求：0=直通 1=合成 2=锁定（抹 B）
    gp_ack: u32,         // 128 DLL 确认：钩子当前状态（== gp_en 才稳定）
    gp_slots: [GpSlot; 4], // 132..212 每槽位 20 字节：en=1 时游戏读到的状态
    gp_inj: u32,         // 212 en=2 时：非 0 = 强制置位 B（回注点按窗口）
}

/// 一个 XInput 槽位的合成状态（与 XINPUT_STATE 逐字段对应）
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpSlot {
    result: u32,   // XInputGetState 返回值（0=成功，1167=未连接）
    packet: u32,   // XINPUT_STATE::packet
    buttons: u16,  // wButtons
    lt: u8,
    rt: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
}

const FLAG_VISIBLE: u32 = 1 << 0;
#[allow(dead_code)] // 宿主侧保留位：纹理尺寸变化时请求重建，DLL 端靠句柄变化判断即可
const FLAG_REBUILD: u32 = 1 << 1;
/// bit2：宿主抓到的位图是**直通 alpha**（非预乘），混合方程要跟着换。
/// PrintWindow 出图的 alpha 语义 Windows 没承诺，宿主每帧实测后把这位置起来。
const FLAG_STRAIGHT: u32 = 1 << 2;

// status：宿主据此在设置里显示人话
const ST_BOOT: u32 = 0;     // 尚未就绪
const ST_READY: u32 = 1;    // 已挂钩且可绘制
const ST_HOOK_ONLY: u32 = 2; // 已挂钩但不是 D3D11，只计帧不绘制
const ST_NO_TEX: u32 = 3;   // 共享纹理打不开
const ST_DRAW_ERR: u32 = 4; // 绘制失败
const ST_NO_HLSL: u32 = 5;  // 找不到 d3dcompiler，无法建着色器
const ST_NO_QUEUE: u32 = 6; // D3D12 游戏：还没抓到游戏的渲染队列（ExecuteCommandLists）

// 绘制路径（Shared.path）：诊断用
const PATH_NONE: u32 = 0;
const PATH_D3D11: u32 = 1;
const PATH_D3D12: u32 = 2;

static SHARED: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());
static P8: AtomicU64 = AtomicU64::new(0);
static P22: AtomicU64 = AtomicU64::new(0);

/* ---- 主交换链跟踪（帧率只测主链） ----
 * 多交换链游戏（星空、部分 UE/RE 引擎）一帧内 Present 多条链：主画面 + UI/副链。
 * 若把所有链的 Present 混在一起量间隔，副链紧随主链的那几毫秒会被当成
 * 「一帧的帧时间」，平均帧率直接虚高数倍（实测星空 400+，1%Low 却正常 144）。
 * 对策：第一个开始 Present 的链认定为主链，只有它的 Present 计帧 + 进帧时间窗口；
 * 主链静默超过 1 秒（切全屏/重建交换链，this 指针换了）才移交主链身份。
 * 注意用 this（交换链对象指针）而非 vtable 区分——同设备建的链共享一张虚表。 */
static MAIN_THIS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static MAIN_LAST_SEEN: AtomicU64 = AtomicU64::new(0);
static FRAMES_MAIN: AtomicU64 = AtomicU64::new(0);

/* ============================== 文件日志 ==============================
 * 注入到别人进程里没有 stdout，出问题只能靠共享内存里那几十个字节猜——「为什么没画」
 * 有十几种可能，猜不动。这里写 %TEMP%\egb_hook_<pid>.log，只记状态变化（不记每帧）。 */
fn dlog_line(msg: std::fmt::Arguments) {
    use std::io::Write;
    // 先拼成整行再一次性写出：多线程（初始化线程 / 游戏渲染线程）同时写同一个文件时，
    // 逐段写会把两行搅在一起，日志就没法读了。
    let mut line = std::fmt::format(msg);
    line.push('\n');
    let path = std::env::temp_dir().join(format!("egb_hook_{}.log", std::process::id()));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

macro_rules! dlog {
    ($($arg:tt)*) => { dlog_line(format_args!($($arg)*)) };
}

/// 同一句话只记一次（诊断日志别把磁盘刷爆）
fn dlog_once(key: &str, msg: std::fmt::Arguments) {
    use std::sync::Mutex;
    static SEEN: std::sync::LazyLock<Mutex<Vec<String>>> =
        std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
    let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if g.iter().any(|k| k == key) {
        return;
    }
    g.push(key.to_string());
    drop(g);
    dlog_line(msg);
}

/// 逐帧步骤痕迹：只接受**字面量**（不建 format_args）。
/// 实测在 Present detour 里用 `format_args!` 带位置参数会在调用点写栈时访问违例
/// （0xC0000005，崩在 egb_hook.dll 自己的指令上）——诊断脚手架一律只写死字符串。
fn trace_lit(step: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n < 60 {
        dlog_lit(&["[trace ", step]);
    }
}

/// 只接受字面量的日志（同 trace_lit 的原因）：手工拼 pid，不碰任何格式化机制
fn dlog_lit(parts: &[&str]) {
    use std::io::Write;
    let mut name = String::from("egb_hook_");
    {
        let mut n = std::process::id();
        let mut digits = [0u8; 10];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        name.push_str(std::str::from_utf8(&digits[i..]).unwrap_or("0"));
    }
    name.push_str(".log");
    let mut path = std::env::temp_dir();
    path.push(name.as_str());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        for p in parts {
            let _ = f.write_all(p.as_bytes());
        }
        let _ = f.write_all(b"\n");
    }
}

/// 一次性日志（只写字面量）
fn dlog_once_lit(step: &str) {
    dlog_lit(&["[once] ", step]);
}

macro_rules! trace {
    ($step:expr) => {
        trace_lit($step)
    };
}

/// 初始化线程上的日志（不跑在游戏渲染线程，可以用格式化）
macro_rules! initlog {
    ($($arg:tt)*) => { dlog_line(format_args!($($arg)*)) };
}

/* ============================== Present detour ============================== */

type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> i32;

// SEH 守护（seh.c 编进本 DLL）：绘制途中任何访问违例都被 __except 拦下，
// 返回异常码而不是把游戏带崩。stable Rust 写不了 __try，故落在 C 侧。
extern "C" {
    fn egb_guard_seh(f: unsafe extern "C" fn(*mut c_void), arg: *mut c_void) -> i32;
}

/// 每张被改写的虚表对应一组原函数。detour 里用 this 的虚表指针回查。
struct Orig {
    vtbl: *mut c_void,
    present: usize,
    present1: usize,
    resize: usize,
    resize1: usize,
}
// 虚表指针在进程内恒定，读写都在单线程安装阶段 + detour 只读，Send 无实际问题
unsafe impl Send for Orig {}
unsafe impl Sync for Orig {}

static ORIGS: std::sync::LazyLock<std::sync::Mutex<Vec<Orig>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 按虚表指针查原函数；查不到（理论上不该发生）退回第一条，至少不把游戏卡死
unsafe fn orig_for(this: *mut c_void) -> (usize, usize) {
    let vtbl = *(this as *mut *mut c_void);
    let v = vtbl as *mut c_void;
    let g = lock(&ORIGS);
    if let Some(o) = g.iter().find(|o| o.vtbl == v) {
        return (o.present, o.present1);
    }
    match g.first() {
        Some(o) => (o.present, o.present1),
        None => (0, 0),
    }
}

/// 按虚表指针回查 ResizeBuffers / ResizeBuffers1 原函数（D3D12 包装资源必须先放手）
unsafe fn orig_resize_for(this: *mut c_void) -> (usize, usize) {
    let vtbl = *(this as *mut *mut c_void);
    let v = vtbl as *mut c_void;
    let g = lock(&ORIGS);
    if let Some(o) = g.iter().find(|o| o.vtbl == v) {
        return (o.resize, o.resize1);
    }
    match g.first() {
        Some(o) => (o.resize, o.resize1),
        None => (0, 0),
    }
}

unsafe fn bump() {
    let p = SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let a = P8.load(Ordering::Relaxed);
    let b = P22.load(Ordering::Relaxed);
    (*p).p8 = a;
    (*p).p22 = b;
    // 帧数只算主链：多交换链游戏（星空等）一帧内 Present 多条链，
    // 全加起来会把帧数灌爆（宿主 Δframes/Δt 跟着虚高）
    (*p).frames = FRAMES_MAIN.load(Ordering::Relaxed);
}

/* ============================== 帧率统计（仿 OptiScaler） ==============================
 * 不依赖任何外部进程：Present detour 里用 QPC（QueryPerformanceCounter）量出
 * 每一帧的真实帧时间，喂进一个滑动窗口（最近 240 帧），每秒回写一次共享内存：
 *   平均 FPS = 1000 / 窗口平均帧时间
 *   1% Low   = 1000 / 窗口内最差 1% 帧时间的均值（PresentMon 同口径）
 * 帧时间直接来自游戏自己的 Present 调用 —— 测谁就是谁，不存在「监控目标
 * 跑到别的进程」的问题（这正是此前交给 HWiNFO 时修不完的坑）。
 *
 * 开销控制：持锁段只有 O(1) 的入队；排序/求均值每秒才发生一次（240 个数），
 * 对渲染线程无可感知影响。帧时间离谱（>500ms = 断点/挂起恢复）不入窗。 */

/// 滑动窗口帧数：240 帧 @60fps ≈ 4 秒，够分位统计稳定又不至于反应迟钝
const FT_RING: usize = 240;

struct FpsState {
    freq: f64,
    last_qpc: u64,
    next_flush: u64,
    ring: std::collections::VecDeque<f64>,
}

static FPS: std::sync::Mutex<Option<FpsState>> = std::sync::Mutex::new(None);

fn flush_fps(st: &FpsState) {
    if st.ring.is_empty() {
        return;
    }
    let n = st.ring.len();
    let avg_ft: f64 = st.ring.iter().sum::<f64>() / n as f64;
    if avg_ft <= 0.0 {
        return;
    }
    // 1% Low：把帧时间按最差（最长）排序，取最差 1%（至少 1 帧）的均值 → 帧率
    let worst_n = (n + 99) / 100;
    let mut sorted: Vec<f64> = st.ring.iter().copied().collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let worst_ft: f64 = sorted[..worst_n].iter().sum::<f64>() / worst_n as f64;
    let p = SHARED.load(Ordering::Relaxed);
    if !p.is_null() {
        unsafe {
            (*p).avg_x100 = (1000.0 / avg_ft * 100.0) as u32;
            (*p).low_x100 = (1000.0 / worst_ft * 100.0) as u32;
            (*p).ft_cnt = n as u32;
        }
    }
}

/// QPC 频率缓存（每秒 tick 数）：KUSER_SHARED_DATA 直读，无系统调用开销
static QPC_FREQ: AtomicU64 = AtomicU64::new(0);
fn qpc_freq() -> u64 {
    let f = QPC_FREQ.load(Ordering::Relaxed);
    if f != 0 {
        return f;
    }
    let mut v = 0i64;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut v);
    }
    let v = if v > 0 { v as u64 } else { 10_000_000 };
    QPC_FREQ.store(v, Ordering::Relaxed);
    v
}

/// Present detour 每帧调用：主链过滤 → 只有主链计帧 + 进帧时间窗口。
/// 副链紧随主链的那几毫秒不再污染帧时间（虚高根因），FRAMES_MAIN 也不再灌爆。
unsafe fn note_frame(this: *mut c_void) {
    let mut now64 = 0i64;
    let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut now64);
    let now = now64 as u64;
    let freq = qpc_freq();
    let main = MAIN_THIS.load(Ordering::Relaxed);
    let last_seen = MAIN_LAST_SEEN.load(Ordering::Relaxed);
    let this_us = this as usize;
    // 主链 = 第一个开始 Present 的链；它静默 ≥1 秒（切全屏/重建交换链后旧链
    // 永远不再 Present）才让位给新链
    let is_main = main == 0
        || main == this_us
        || (last_seen != 0 && now.saturating_sub(last_seen) >= freq);
    if !is_main {
        return;
    }
    if main != this_us {
        MAIN_THIS.store(this_us, Ordering::Relaxed);
        // 主链易主：帧时间序列作废，别拿旧链的 last_qpc 和新链的首帧做差
        let mut g = lock(&FPS);
        if let Some(s) = g.as_mut() {
            s.last_qpc = 0;
        }
    }
    MAIN_LAST_SEEN.store(now, Ordering::Relaxed);
    FRAMES_MAIN.fetch_add(1, Ordering::Relaxed);
    let mut g = lock(&FPS);
    let st = g.get_or_insert_with(|| FpsState {
        freq: freq as f64,
        last_qpc: 0,
        next_flush: 0,
        ring: std::collections::VecDeque::with_capacity(FT_RING + 1),
    });
    if st.last_qpc != 0 {
        let dt_ms = (now - st.last_qpc) as f64 / st.freq * 1000.0;
        if dt_ms > 0.0 && dt_ms < 500.0 {
            if st.ring.len() >= FT_RING {
                st.ring.pop_front();
            }
            st.ring.push_back(dt_ms);
        }
    }
    st.last_qpc = now;
    if now >= st.next_flush {
        // QPC 频率就是每秒 tick 数 → +freq 即「1 秒后」
        st.next_flush = now + st.freq as u64;
        flush_fps(st);
    }
}

unsafe extern "system" fn present_detour(
    this: *mut c_void,
    sync_interval: u32,
    flags: u32,
) -> i32 {
    let n = P8.fetch_add(1, Ordering::Relaxed);
    if n < 3 {
        dlog_once_lit("Present 已被 hook 命中（首次）");
    }
    note_frame(this);
    bump();
    draw_overlay(this);
    let (orig, _) = orig_for(this);
    if orig == 0 {
        dlog_once_lit("查不到原函数 → 不转发真实 Present");
        return 0;
    }
    let f: PresentFn = std::mem::transmute::<usize, PresentFn>(orig);
    f(this, sync_interval, flags)
}

unsafe extern "system" fn present1_detour(
    this: *mut c_void,
    sync_interval: u32,
    present_flags: u32,
    params: *const c_void,
) -> i32 {
    P22.fetch_add(1, Ordering::Relaxed);
    note_frame(this);
    bump();
    draw_overlay(this);
    let (_, orig1) = orig_for(this);
    if orig1 == 0 {
        return 0;
    }
    let f: Present1Fn = std::mem::transmute::<usize, Present1Fn>(orig1);
    f(this, sync_interval, present_flags, params)
}

/* ============================== ResizeBuffers（D3D12 包装资源必须放手） ==============================
 * D3D12 路径用 D3D11On12 把后备缓冲包装成 D3D11 纹理，包装对象持有 D3D12 资源的引用。
 * 只要我们还握着这个引用，游戏的 ResizeBuffers 就会因为「后备缓冲还有未释放的引用」
 * 直接失败（切分辨率 / 切全屏 / HDR 切换都会走到这里）。所以在转发前先把包装资源全放开。 */

type ResizeBuffersFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32) -> i32;
type ResizeBuffers1Fn =
    unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32, *const u32, *const *mut c_void) -> i32;

unsafe extern "system" fn resize_detour(
    this: *mut c_void,
    count: u32,
    w: u32,
    h: u32,
    fmt: u32,
    flags: u32,
) -> i32 {
    dlog_once_lit("游戏调用 ResizeBuffers → 先放开 D3D12 包装资源");
    release_on12();
    let (orig, _) = orig_resize_for(this);
    if orig == 0 {
        return 0;
    }
    let f: ResizeBuffersFn = std::mem::transmute::<usize, ResizeBuffersFn>(orig);
    f(this, count, w, h, fmt, flags)
}

unsafe extern "system" fn resize1_detour(
    this: *mut c_void,
    count: u32,
    w: u32,
    h: u32,
    fmt: u32,
    flags: u32,
    nodes: *const u32,
    queues: *const *mut c_void,
) -> i32 {
    dlog_once_lit("游戏调用 ResizeBuffers1 → 先放开 D3D12 包装资源");
    release_on12();
    let (_, orig1) = orig_resize_for(this);
    if orig1 == 0 {
        return 0;
    }
    let f: ResizeBuffers1Fn = std::mem::transmute::<usize, ResizeBuffers1Fn>(orig1);
    f(this, count, w, h, fmt, flags, nodes, queues)
}

/* ============================== D3D12 渲染队列捕获 ==============================
 * D3D11On12 必须拿「游戏的渲染队列」提交覆盖层工作：用别的队列就得自己跟游戏队列
 * 做跨队列同步（拿不到游戏队列的围栏，写后备缓冲必然竞争）。
 * 交换链本身不给队列（实测 GetDevice(ID3D12CommandQueue) 返回 E_NOINTERFACE），
 * 所以改写 ID3D12CommandQueue::ExecuteCommandLists（槽位 10）：Present 之前最后提交
 * 工作的 DIRECT 队列就是渲染队列。 */

type ExecListsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);

/// 已确认的 DIRECT 渲染队列（Present 时用它）
static DIRECT_QUEUE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// 最近一个非 DIRECT 队列：记下来免得每帧重复查询类型
static OTHER_QUEUE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static QUEUE_ORIG: AtomicU64 = AtomicU64::new(0);

unsafe fn direct_queue() -> Option<ID3D12CommandQueue> {
    let q = DIRECT_QUEUE.load(Ordering::Relaxed);
    if q.is_null() {
        return None;
    }
    // 借来的包装：调用方必须 mem::forget，别 Release 掉游戏的队列
    Some(queue_borrowed(q))
}

unsafe fn is_direct_queue(q: *mut c_void) -> bool {
    if q.is_null() {
        return false;
    }
    let queue = queue_borrowed(q);
    let direct = queue.GetDesc().Type == D3D12_COMMAND_LIST_TYPE_DIRECT;
    std::mem::forget(queue);
    direct
}

unsafe extern "system" fn exec_lists_detour(
    this: *mut c_void,
    num: u32,
    lists: *const *mut c_void,
) {
    // 渲染队列一旦认定就冻结：游戏常有多条 DIRECT 队列（主渲染 + 拷贝/上传等），
    // 若每帧把 DIRECT_QUEUE 覆盖成新指针，draw_overlay_12 的缓存键（queue_ptr）就会
    // 每帧失配 → 每帧重建 D3D11On12 设备（在 Present 调用栈里做重活）→ 星空加载卡死。
    // 只在尚未捕获任何队列时才认第一条 DIRECT。
    if DIRECT_QUEUE.load(Ordering::Relaxed).is_null()
        && this != OTHER_QUEUE.load(Ordering::Relaxed)
        && is_direct_queue(this)
    {
        DIRECT_QUEUE.store(this, Ordering::Relaxed);
        dlog_lit(&["[once] 冻结 D3D12 DIRECT 渲染队列 @", "0x"]);
    } else if OTHER_QUEUE.load(Ordering::Relaxed).is_null()
        && this != DIRECT_QUEUE.load(Ordering::Relaxed)
        && !is_direct_queue(this)
    {
        OTHER_QUEUE.store(this, Ordering::Relaxed);
    }
    let orig = QUEUE_ORIG.load(Ordering::Relaxed) as usize;
    if orig != 0 {
        let f: ExecListsFn = std::mem::transmute::<usize, ExecListsFn>(orig);
        f(this, num, lists);
    }
}

/// 用**游戏自己的 D3D12 设备**建一条哑队列拿虚表 → 改写 ExecuteCommandLists。
/// 为什么不用 D3D12CreateDevice 自建设备：实测在已经跑着 D3D12 的进程里，
/// D3D12CreateDevice 会返回 0x887A002B（DXGI_ERROR_ACCESS_DENIED），钩子就装不上。
/// 设备从哪来？交换链的后备缓冲是 ID3D12Resource，`GetDevice` 反查就是游戏自己的设备
/// （ID3D12DeviceChild），实测可用——比自建设备可靠，也不用额外占显存。
/// 返回成功改写的槽位数（0 = 装不上）。
unsafe fn hook_queue_vtable_of(dev12: &ID3D12Device) -> u32 {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC,
    };
    let queue: ID3D12CommandQueue = match dev12.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        ..Default::default()
    }) {
        Ok(q) => q,
        Err(_) => {
            dlog_once_lit("用游戏设备建哑队列失败");
            return 0;
        }
    };
    let vtbl = vtable_of(queue.as_raw());
    // 槽位 10 = ID3D12CommandQueue::ExecuteCommandLists
    let orig = std::ptr::read(vtbl.add(10) as *mut usize);
    if orig == exec_lists_detour as *const () as usize {
        std::mem::forget(queue);
        return 1;
    }
    if orig == 0 {
        std::mem::forget(queue);
        return 0;
    }
    QUEUE_ORIG.store(orig as u64, Ordering::Relaxed);
    let ok = patch_slot(vtbl, 10, exec_lists_detour as *mut c_void);
    if ok {
        dlog_once_lit("队列虚表槽位 10 已改写");
    } else {
        dlog_once_lit("队列虚表槽位 10 改写失败");
    }
    // 队列必须泄漏：虚表地址属于 D3D12Core.dll，释放后地址可能失效
    std::mem::forget(queue);
    ok as u32
}

/// 惰性装 D3D12 队列钩子（每帧调用，装过就直接返回）。
/// 队列要被「下一次 ExecuteCommandLists」才会被认出来，所以这一帧多半还画不了。
unsafe fn ensure_queue_hook(this: *mut c_void) -> bool {
    if direct_queue().is_some() {
        return true;
    }
    // 建哑队列会占资源：只试几次，失败就放弃（每帧重试没意义，还会一直占显存）
    static TRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if TRIES.load(Ordering::Relaxed) >= 3 {
        return false;
    }
    TRIES.fetch_add(1, Ordering::Relaxed);
    let Some(bb) = raw_get_buffer(this, 0, &ID3D12Resource::IID) else {
        dlog_once_lit("拿不到后备缓冲 → 装不了队列钩子");
        return false;
    };
    trace!("qz-bb");
    let Some(dev) = raw_get_device(bb, 7, &ID3D12Device::IID) else {
        dlog_once_lit("后备缓冲反查 D3D12 设备失败");
        return false;
    };
    trace!("qz-dev");
    let dev12: ID3D12Device = borrowed(dev);
    let n = hook_queue_vtable_of(&dev12);
    std::mem::forget(dev12);
    trace!("qz-patch");
    let p = SHARED.load(Ordering::Relaxed);
    if !p.is_null() {
        (*p).hook_q = n;
    }
    n > 0
}

/* ============================== 虚表改写 ============================== */

/* ============================== 借来的 COM 接口包装 ==============================
 * windows-rs 的接口类型是 `#[repr(transparent)] struct X(NonNull<c_void>)`——**包装本身
 * 就是那 8 字节的接口指针**。所以绝不能用 `&*(this as *const X)` 去「重新解释」对象地址：
 * 那样包装的地址 = 对象地址，crate 内部 `as_raw()` 取到的就是对象首字段（虚表指针），
 * 于是它拿虚表指针当接口指针用 → `call [虚表首槽 + 偏移]` → 访问违例（实测崩溃点）。
 * 正确做法：把接口指针**值**位拷贝进包装（零开销），用完 `mem::forget`（借来的引用不
 * 能 Release，否则会把游戏的对象引用计数减掉）。 */

unsafe fn borrowed<T: windows::core::Interface>(ptr: *mut c_void) -> T {
    std::mem::transmute_copy::<*mut c_void, T>(&ptr)
}

unsafe fn queue_borrowed(q: *mut c_void) -> ID3D12CommandQueue {
    borrowed::<ID3D12CommandQueue>(q)
}

/// 取 COM 对象的虚表指针：**对象首字段才是虚表**（`*(对象) = 虚表`）。
/// 少了这一层解引用，后面 `vtbl[i] = detour` 写的就是对象内部的字段——症状极其隐蔽：
/// hooked 有值、Present 一次都收不到，还顺手破坏了交换链对象的内存。别删这一层。
unsafe fn vtable_of(obj: *mut c_void) -> *mut *mut c_void {
    *(obj as *mut *mut *mut c_void)
}

/// 槽位函数所在模块守卫：真 D3D 对象的虚表函数只可能来自系统 D3D 模块——
/// D3D11 家族 = d3d11.dll / d3d11on12.dll；D3D12 家族 = d3d12.dll / D3D12Core.dll
/// （Agility SDK 的实现在游戏目录的 D3D12Core.dll 里，必须放行）。
/// 星空实测：OptiScaler（dxgi.dll 代理）包装交换链后，GetBuffer 返回的
/// 「D3D11 纹理」虚表槽 3 根本不是 GetDevice，直接调用就是访问违例闪退。
/// 模块对不上 → 这个对象是包装出来的假货，不能碰。
unsafe fn slot_fn_module_ok(obj: *mut c_void, slot: usize, want12: bool) -> bool {
    if obj.is_null() {
        return false;
    }
    let vtbl = vtable_of(obj);
    let f = std::ptr::read(vtbl.add(slot) as *mut usize) as *const u16;
    if f.is_null() {
        return false;
    }
    let mut hmod = HMODULE::default();
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    if GetModuleHandleExW(flags, PCWSTR(f), &mut hmod).is_err() || hmod.is_invalid() {
        return false;
    }
    let mut name = [0u16; 260];
    let n = GetModuleFileNameW(Some(hmod), &mut name) as usize;
    let path = String::from_utf16_lossy(&name[..n.min(260)]).to_ascii_lowercase();
    if want12 {
        path.ends_with("d3d12.dll") || path.ends_with("d3d12core.dll")
    } else {
        path.ends_with("d3d11.dll") || path.ends_with("d3d11on12.dll")
    }
}

/// 函数地址是否落在**系统** dxgi.dll（C:\Windows\System32）。
/// OptiScaler 之类会用自己包装的交换链顶替游戏的链：那条链的后备缓冲由代理管理
/// （状态迁移、围栏都是它在做），我们再包一层去画就是两家抢同一批资源——
/// 星空实测表现为 ntdll 堆破坏闪退（崩在游戏的 malloc/free，不在我们模块里）。
/// 判定：Present 原函数来自非系统 dxgi → 代理链 → 只计帧不绘制。
unsafe fn fn_in_system_dxgi(addr: usize) -> bool {
    if addr == 0 {
        return false;
    }
    // 系统目录（小写）只取一次
    static SYS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let mut buf = [0u16; 260];
        let n = unsafe {
            windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut buf))
        } as usize;
        String::from_utf16_lossy(&buf[..n.min(260)]).to_ascii_lowercase()
    });
    let mut hmod = HMODULE::default();
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    if GetModuleHandleExW(flags, PCWSTR(addr as *const u16), &mut hmod).is_err() {
        return false;
    }
    let mut name = [0u16; 260];
    let n = GetModuleFileNameW(Some(hmod), &mut name) as usize;
    let path = String::from_utf16_lossy(&name[..n.min(260)]).to_ascii_lowercase();
    path.starts_with(SYS.as_str()) && path.ends_with("dxgi.dll")
}

/* ============================== 在 detour 里的原始虚表调用 ==============================
 * 绘制的第一步（拿后备缓冲、反查设备）必须用**手写的那一次解引用**：
 * 实测在 Present detour 里走 windows-rs 的 `sc.GetBuffer(...)` 会访问违例——反汇编显示它
 * 把「虚表首槽（QueryInterface）」当成了虚表基址再取 +0x48 调用（0xC0000005，崩在本 DLL
 * 的 call 指令上）。手写虚表调用没有这个歧义，也不依赖 crate 的代码生成。 */

type GetBufferFn = unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void) -> i32;
type GetDeviceFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;

/// IDXGISwapChain::GetBuffer（虚表槽位 9）
unsafe fn raw_get_buffer(this: *mut c_void, index: u32, iid: *const GUID) -> Option<*mut c_void> {
    if this.is_null() {
        return None;
    }
    let vtbl = vtable_of(this);
    let f: GetBufferFn = std::mem::transmute(*(vtbl.add(9) as *mut usize));
    let mut out: *mut c_void = std::ptr::null_mut();
    if f(this, index, iid, &mut out) < 0 || out.is_null() {
        return None;
    }
    Some(out)
}

/// ID3D11DeviceChild::GetDevice（虚表槽位 3）——**只收一个出参**，没有 riid！
/// （ID3D12DeviceChild::GetDevice 是 (riid, ppv) 两个参数，两边签名不一样，别写成一个。）
unsafe fn raw_get_device11(this: *mut c_void) -> Option<*mut c_void> {
    if this.is_null() {
        return None;
    }
    // 守卫：虚表槽 3 的函数必须来自 d3d11 系模块。代理层（OptiScaler 等）包装
    // 出来的假纹理槽 3 不是 GetDevice，调了就是闪退（星空实测），直接拒绝。
    if !slot_fn_module_ok(this, 3, false) {
        dlog_once_lit("D3D11 槽位模块守卫拒绝（代理包装的假纹理）→ 不调 GetDevice");
        return None;
    }
    type F = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32;
    let vtbl = vtable_of(this);
    let f: F = std::mem::transmute(*(vtbl.add(3) as *mut usize));
    let mut out: *mut c_void = std::ptr::null_mut();
    if f(this, &mut out) < 0 || out.is_null() {
        return None;
    }
    Some(out)
}

/// ID3D12DeviceChild::GetDevice（虚表槽位 7，(riid, ppv)）
unsafe fn raw_get_device(this: *mut c_void, slot: usize, iid: *const GUID) -> Option<*mut c_void> {
    if this.is_null() {
        return None;
    }
    // 同款守卫：D3D12 对象的虚表函数必须来自 d3d12.dll 或 D3D12Core.dll
    if !slot_fn_module_ok(this, slot, true) {
        dlog_once_lit("D3D12 槽位模块守卫拒绝 → 不调 GetDevice");
        return None;
    }
    let vtbl = vtable_of(this);
    let f: GetDeviceFn = std::mem::transmute(*(vtbl.add(slot) as *mut usize));
    let mut out: *mut c_void = std::ptr::null_mut();
    if f(this, iid, &mut out) < 0 || out.is_null() {
        return None;
    }
    Some(out)
}

/// 改写虚表某个槽位：先 VirtualProtect 开写权限，写完还原
unsafe fn patch_slot(vtbl: *mut *mut c_void, idx: usize, new_fn: *mut c_void) -> bool {
    let slot: *mut c_void = vtbl.add(idx) as *mut c_void;
    let mut old = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(
        slot.cast::<c_void>(),
        std::mem::size_of::<*mut c_void>(),
        PAGE_READWRITE,
        &mut old,
    )
    .is_err()
    {
        return false;
    }
    let slot_ptr = slot as *mut *mut c_void;
    std::ptr::write(slot_ptr, new_fn);
    let mut _tmp = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(
        slot.cast::<c_void>(),
        std::mem::size_of::<*mut c_void>(),
        old,
        &mut _tmp,
    );
    true
}

unsafe fn record_orig(vtbl: *mut *mut c_void) -> usize {
    let v = vtbl as *mut c_void;
    let p = std::ptr::read(vtbl.add(8) as *mut usize);
    // 槽位地址判据：同一个模块里的两个函数不可能相距几 MB 以上。blt 那套虚表在老系统上
    // 只到槽位 17，读 22/36 就是越界读 .rdata；用它把「这个槽位不是函数指针」挡掉，
    // 顺便保证只去 patch 我们真的记下原函数的槽位（否则转发时无路可走）。
    let sane = |x: usize| x != 0 && p != 0 && x.abs_diff(p) < 0x0100_0000;
    let rd = |i: usize| std::ptr::read(vtbl.add(i) as *mut usize);
    let p1 = rd(22);
    let rsz = rd(13);
    let rsz1 = rd(36);
    let mut g = lock(&ORIGS);
    if !g.iter().any(|o| o.vtbl == v) {
        g.push(Orig {
            vtbl: v,
            present: p,
            present1: if sane(p1) { p1 } else { 0 },
            resize: if sane(rsz) { rsz } else { 0 },
            resize1: if sane(rsz1) { rsz1 } else { 0 },
        });
    }
    g.len()
}

/// 这张虚表登记过的原函数（没登记过返回全 0）
unsafe fn orig_parts_of(vtbl: *mut *mut c_void) -> Orig {
    let v = vtbl as *mut c_void;
    let g = lock(&ORIGS);
    match g.iter().find(|o| o.vtbl == v) {
        Some(o) => Orig {
            vtbl: o.vtbl,
            present: o.present,
            present1: o.present1,
            resize: o.resize,
            resize1: o.resize1,
        },
        None => Orig {
            vtbl: v,
            present: 0,
            present1: 0,
            resize: 0,
            resize1: 0,
        },
    }
}

/// 建哑交换链并把 Present(8) / Present1(22) / ResizeBuffers(13) / ResizeBuffers1(36)
/// 改写成 detour。flip=true 表示这是 flip-model（IDXGISwapChain1+）那套虚表。
/// 返回成功改写的槽位数。
unsafe fn hook_vtable(vtbl: *mut *mut c_void, flip: bool) -> u32 {
    record_orig(vtbl);
    let orig = orig_parts_of(vtbl);
    let mut n = 0u32;
    let mut patch = |idx: usize, cur: usize, new_fn: *mut c_void| -> u32 {
        if cur != new_fn as usize && patch_slot(vtbl, idx, new_fn) {
            1
        } else {
            0
        }
    };
    // 已经是我们自己的 detour（重复注入 / 同一虚表被两条哑链覆盖）就不再改，
    // 否则会把 detour 当成原函数记下来，形成自递归
    let cur8 = std::ptr::read(vtbl.add(8) as *mut usize);
    n += patch(8, cur8, present_detour as *mut c_void);
    if flip && orig.present1 != 0 {
        let cur22 = std::ptr::read(vtbl.add(22) as *mut usize);
        n += patch(22, cur22, present1_detour as *mut c_void);
    }
    // ResizeBuffers：不装的话 D3D12 的包装资源会挡死游戏的切分辨率/切全屏
    if orig.resize != 0 {
        let cur13 = std::ptr::read(vtbl.add(13) as *mut usize);
        n += patch(13, cur13, resize_detour as *mut c_void);
    }
    if flip && orig.resize1 != 0 {
        let cur36 = std::ptr::read(vtbl.add(36) as *mut usize);
        n += patch(36, cur36, resize1_detour as *mut c_void);
    }
    dlog!(
        "[hook] 虚表 {:p}（flip={flip}）原函数 present={:#x} present1={:#x} resize={:#x} resize1={:#x} → 改写 {n} 个槽位",
        vtbl,
        orig.present,
        orig.present1,
        orig.resize,
        orig.resize1
    );
    n
}

unsafe fn make_dummy_window() -> Option<HWND> {
    let hinstance: HINSTANCE = HINSTANCE(GetModuleHandleW(PCWSTR::null()).ok()?.0);
    CreateWindowExW(
        Default::default(),
        w!("STATIC"),
        w!(""),
        WS_OVERLAPPED,
        0,
        0,
        8,
        8,
        None,
        None,
        Some(hinstance),
        None,
    )
    .ok()
}

fn sample_desc() -> DXGI_SAMPLE_DESC {
    DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }
}

/// blt-model 交换链（CreateSwapChain，IDXGISwapChain）
unsafe fn hook_blt_swapchain(hwnd: HWND) -> u32 {
    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: 8,
            Height: 8,
            RefreshRate: DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 0,
            },
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
            Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
        },
        SampleDesc: sample_desc(),
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: BOOL(1),
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        Flags: 0,
    };
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut sc: Option<IDXGISwapChain> = None;
    let mut dev: Option<ID3D11Device> = None;
    if D3D11CreateDeviceAndSwapChain(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_FLAG(0),
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&desc),
        Some(&mut sc),
        Some(&mut dev),
        None,
        None,
    )
    .is_err()
    {
        return 0;
    }
    let Some(sc) = sc else { return 0 };
    let vtbl = vtable_of(sc.as_raw());
    let n = hook_vtable(vtbl, false);
    // 设备与交换链必须泄漏：虚表地址随 dxgi 模块存在，释放后地址可能失效
    std::mem::forget(sc);
    if let Some(d) = dev {
        std::mem::forget(d);
    }
    n
}

/// flip-model 交换链（CreateSwapChainForHwnd，IDXGISwapChain1）——D3D12 / 现代
/// 游戏几乎都走这条，漏了它就一条 Present 都收不到
unsafe fn hook_flip_swapchain(hwnd: HWND) -> u32 {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    if D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_FLAG(0),
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        Some(&mut ctx),
    )
    .is_err()
    {
        initlog!("[hook] flip：D3D11CreateDevice 失败");
        return 0;
    }
    let Some(dev) = dev else { return 0 };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 8,
        Height: 8,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: BOOL(0),
        SampleDesc: sample_desc(),
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        // 必须是 UNSPECIFIED：CreateSwapChainForHwnd 只接受这个值，PREMULTIPLIED 会让
        // 创建直接失败 → flip 虚表根本没被打上补丁 → 现代游戏（DX12/flip）一条 Present
        // 都收不到。这是「帧计数恒为 0、覆盖层不生效」的根因，别再改回去。
        AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
        Flags: 0,
    };
    let mut made: Option<IDXGISwapChain1> = None;
    // 优先用**系统 dxgi.dll** 的原生工厂造哑交换链：系统 flip 虚表是「必经之路」——
    // 游戏交换链就算被 dxgi.dll 代理（OptiScaler 之类）包装，最终也要对真实交换链调
    // Present，patch 系统虚表两种情况都拦得到；patch 代理虚表只拦得到包装情形。
    // 注意必须用完整路径加载：进程里已有一个名叫 dxgi.dll 的代理模块，裸名字会拿到它。
    if let Some(real) = LoadLibraryW(w!("C:\\Windows\\System32\\dxgi.dll")).ok() {
        if let Some(p) = GetProcAddress(real, s!("CreateDXGIFactory1")) {
            type CreateFactory1Fn = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> i32;
            let f: CreateFactory1Fn = std::mem::transmute(p);
            let mut raw: *mut c_void = std::ptr::null_mut();
            if f(&IDXGIFactory2::IID, &mut raw) >= 0 && !raw.is_null() {
                let real_factory: IDXGIFactory2 = borrowed(raw);
                if let Ok(sc1) = real_factory.CreateSwapChainForHwnd(
                    &dev,
                    hwnd,
                    &desc,
                    None,
                    None::<&windows::Win32::Graphics::Dxgi::IDXGIOutput>,
                ) {
                    made = Some(sc1);
                    std::mem::forget(real_factory);
                } else {
                    initlog!("[hook] flip：系统 dxgi 工厂拒绝哑交换链");
                }
            } else {
                initlog!("[hook] flip：系统 CreateDXGIFactory1 调用失败");
            }
        } else {
            initlog!("[hook] flip：系统 dxgi.dll 里找不到 CreateDXGIFactory1");
        }
    } else {
        initlog!("[hook] flip：加载系统 dxgi.dll 失败");
    }
    if made.is_none() {
        // 兜底：走进程内（可能是代理的）工厂再试一次
        initlog!("[hook] flip：改用进程内 dxgi 工厂重试");
        if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory2>() {
            if let Ok(sc1) = factory.CreateSwapChainForHwnd(
                &dev,
                hwnd,
                &desc,
                None,
                None::<&windows::Win32::Graphics::Dxgi::IDXGIOutput>,
            ) {
                made = Some(sc1);
            } else {
                initlog!("[hook] flip：进程内工厂也拒绝哑交换链");
            }
        } else {
            initlog!("[hook] flip：CreateDXGIFactory1 失败");
        }
    }
    let Some(sc1) = made else {
        return 0;
    };
    let vtbl = vtable_of(sc1.as_raw());
    let n = hook_vtable(vtbl, true);
    std::mem::forget(sc1);
    std::mem::forget(dev);
    if let Some(c) = ctx {
        std::mem::forget(c);
    }
    n
}

/* ============================== 着色器（运行时编译） ============================== */

type D3DCompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    *const u8,
    *const c_void,
    *const c_void,
    *const u8,
    *const u8,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> i32;

const VS_SRC: &str = "\
struct I { float3 p : POSITION; float2 t : TEXCOORD; };
struct O { float4 p : SV_POSITION; float2 t : TEXCOORD; };
O main(I i) { O o; o.p = float4(i.p.xy, 0.f, 1.f); o.t = i.t; return o; }\0";

const PS_SRC: &str = "\
Texture2D g_tex : register(t0);
SamplerState g_samp : register(s0);
float4 main(float4 p : SV_POSITION, float2 t : TEXCOORD) : SV_TARGET {
    return g_tex.Sample(g_samp, t);
}\0";

/// d3dcompiler_47.dll 随 Windows 10/11 系统自带；动态加载，避免链接期依赖 SDK
unsafe fn d3d_compile(
    src: &str,
    entry: &str,
    target: &str,
) -> Option<(Vec<u8>, String)> {
    let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(w!("d3dcompiler_47.dll")).ok()?;
    let proc = windows::Win32::System::LibraryLoader::GetProcAddress(
        lib,
        windows::core::PCSTR(b"D3DCompile\0".as_ptr()),
    )?;
    let compile: D3DCompileFn = std::mem::transmute::<*const c_void, D3DCompileFn>(proc as *const _);
    let mut code: *mut c_void = std::ptr::null_mut();
    let mut err: *mut c_void = std::ptr::null_mut();
    let hr = compile(
        src.as_ptr() as *const c_void,
        src.len(),
        b"egb\0".as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        entry.as_ptr(),
        target.as_ptr(),
        0,
        0,
        &mut code,
        &mut err,
    );
    if hr != 0 || code.is_null() {
        let msg = if !err.is_null() {
            let b = ID3DBlob::from_raw(err as *mut _);
            let p = b.GetBufferPointer();
            let n = b.GetBufferSize();
            let s = std::slice::from_raw_parts(p as *const u8, n);
            String::from_utf8_lossy(s).to_string()
        } else {
            format!("HRESULT {hr:#x}")
        };
        return Some((Vec::new(), msg));
    }
    let blob = ID3DBlob::from_raw(code as *mut _);
    let p = blob.GetBufferPointer();
    let n = blob.GetBufferSize();
    let out = std::slice::from_raw_parts(p as *const u8, n).to_vec();
    Some((out, String::new()))
}

/* ============================== 绘制资源 ============================== */

/// 一个顶点：NDC 坐标 + UV
#[repr(C)]
#[derive(Clone, Copy)]
struct Vert {
    x: f32,
    y: f32,
    _z: f32,
    u: f32,
    v: f32,
}

struct Render {
    dev_ptr: usize,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    vb: ID3D11Buffer,
    /// 位图是预乘 alpha 时用它：src 已乘过 alpha，直接加
    blend_premul: ID3D11BlendState,
    /// 位图是直通 alpha 时用它：src 还要乘一次 alpha
    blend_straight: ID3D11BlendState,
    samp: ID3D11SamplerState,
    srv: Option<ID3D11ShaderResourceView>,
    tex: Option<ID3D11Texture2D>,
    tex_handle: u64,
    /// 顶点缓存内容签名（位置/尺寸/缓冲尺寸），变了才重写顶点
    last_rect: (i32, i32, u32, u32, u32, u32),
}

static RENDER: std::sync::LazyLock<std::sync::Mutex<Option<Render>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));
/// 绘制一旦失败就别每帧重试（每帧建资源会把游戏拖垮）
static DRAW_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe fn build_render(dev: &ID3D11Device, vs_code: &[u8], ps_code: &[u8]) -> Option<Render> {
    let mut vs: Option<ID3D11VertexShader> = None;
    dev.CreateVertexShader(vs_code, None, Some(&mut vs)).ok()?;
    let vs = vs?;
    let mut ps: Option<ID3D11PixelShader> = None;
    dev.CreatePixelShader(ps_code, None, Some(&mut ps)).ok()?;
    let ps = ps?;

    let elems: [D3D11_INPUT_ELEMENT_DESC; 2] = [
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(b"POSITION\0".as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32B32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(b"TEXCOORD\0".as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 12,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];
    let mut layout: Option<ID3D11InputLayout> = None;
    dev.CreateInputLayout(&elems, vs_code, Some(&mut layout)).ok()?;
    let layout = layout?;

    let stride = std::mem::size_of::<Vert>() as u32;
    let bd = D3D11_BUFFER_DESC {
        ByteWidth: stride * 4,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let init = [Vert { x: 0.0, y: 0.0, _z: 0.0, u: 0.0, v: 0.0 }; 4];
    let sd = D3D11_SUBRESOURCE_DATA {
        pSysMem: init.as_ptr() as *const c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut vb: Option<ID3D11Buffer> = None;
    dev.CreateBuffer(&bd, Some(&sd), Some(&mut vb)).ok()?;
    let vb = vb?;

    // 两套混合方程，按位图的 alpha 语义二选一：
    //   预乘  src 已含 alpha → SrcBlend = ONE
    //   直通  src 未乘 alpha → SrcBlend = SRC_ALPHA
    // 选错的症状很典型：预乘按直通混 → 半透明处发灰发亮；直通按预乘混 → 过曝成白块。
    fn mk(bdesc: &mut D3D11_BLEND_DESC, src: D3D11_BLEND) {
        bdesc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: BOOL(1),
            SrcBlend: src,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
    }
    let mut bdesc = D3D11_BLEND_DESC::default();
    bdesc.AlphaToCoverageEnable = BOOL(0);
    bdesc.IndependentBlendEnable = BOOL(0);
    mk(&mut bdesc, D3D11_BLEND_ONE);
    let mut blend_premul: Option<ID3D11BlendState> = None;
    dev.CreateBlendState(&bdesc, Some(&mut blend_premul)).ok()?;
    mk(&mut bdesc, D3D11_BLEND_SRC_ALPHA);
    let mut blend_straight: Option<ID3D11BlendState> = None;
    dev.CreateBlendState(&bdesc, Some(&mut blend_straight)).ok()?;
    let (blend_premul, blend_straight) = (blend_premul?, blend_straight?);

    let sdesc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: 0.0,
    };
    let mut samp: Option<ID3D11SamplerState> = None;
    dev.CreateSamplerState(&sdesc, Some(&mut samp)).ok()?;
    let samp = samp?;

    Some(Render {
        dev_ptr: dev.as_raw() as usize,
        vs,
        ps,
        layout,
        vb,
        blend_premul,
        blend_straight,
        samp,
        srv: None,
        tex: None,
        tex_handle: 0,
        last_rect: (0, 0, 0, 0, 0, 0),
    })
}

/// 打开宿主的共享纹理（句柄变了才重开）
unsafe fn ensure_texture(dev: &ID3D11Device, r: &mut Render, handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    if r.tex_handle == handle && r.srv.is_some() {
        return true;
    }
    r.srv = None;
    r.tex = None;
    r.tex_handle = 0;
    let mut tex: Option<ID3D11Texture2D> = None;
    let hr = dev.OpenSharedResource::<ID3D11Texture2D>(
        HANDLE(handle as *mut c_void),
        &mut tex as *mut Option<ID3D11Texture2D>,
    );
    if hr.is_err() {
        return false;
    }
    let Some(tex) = tex else { return false };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    let res: ID3D11Resource = tex.cast().unwrap();
    if dev
        .CreateShaderResourceView(&res, None, Some(&mut srv))
        .is_err()
    {
        return false;
    }
    r.srv = srv;
    r.tex = Some(tex);
    r.tex_handle = handle;
    true
}

unsafe fn write_verts(ctx: &ID3D11DeviceContext, r: &mut Render, v: &[Vert; 4]) {
    let mut ms = D3D11_MAPPED_SUBRESOURCE::default();
    if ctx
        .Map(
            &r.vb.cast::<ID3D11Resource>().unwrap(),
            0,
            D3D11_MAP_WRITE_DISCARD,
            0,
            Some(&mut ms),
        )
        .is_err()
    {
        return;
    }
    std::ptr::copy_nonoverlapping(
        v.as_ptr() as *const u8,
        ms.pData as *mut u8,
        std::mem::size_of::<Vert>() * 4,
    );
    ctx.Unmap(&r.vb.cast::<ID3D11Resource>().unwrap(), 0);
}

/// 状态保存/还原：绘制完必须把游戏原本的管线状态原样放回去，
/// 否则下一帧游戏自己的Draw 会用我们残留的 RT / 视口 / 着色器，直接花屏。
struct Saved {
    rtv: Option<ID3D11RenderTargetView>,
    dsv: Option<ID3D11DepthStencilView>,
    vp_n: u32,
    vp: [D3D11_VIEWPORT; 4],
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    sample_mask: u32,
    ds_state: Option<ID3D11DepthStencilState>,
    stencil: u32,
    topo: D3D_PRIMITIVE_TOPOLOGY,
    layout: Option<ID3D11InputLayout>,
    vb: Option<ID3D11Buffer>,
    vb_stride: u32,
    vb_off: u32,
    vs: Option<ID3D11VertexShader>,
    ps: Option<ID3D11PixelShader>,
    srv: Option<ID3D11ShaderResourceView>,
    sampler: Option<ID3D11SamplerState>,
}

/* ============================== 绘制入口 ============================== */

/// 着色器字节码（只编译一次，两条绘制路径共用）
static SHADERS: std::sync::OnceLock<Option<(Vec<u8>, Vec<u8>)>> = std::sync::OnceLock::new();

fn shader_bytes() -> Option<&'static (Vec<u8>, Vec<u8>)> {
    SHADERS
        .get_or_init(|| {
            let vs = unsafe { d3d_compile(VS_SRC, "main\0", "vs_5_0\0") };
            let ps = unsafe { d3d_compile(PS_SRC, "main\0", "ps_5_0\0") };
            match (vs, ps) {
                (Some((v, _)), Some((q, _))) if !v.is_empty() && !q.is_empty() => Some((v, q)),
                _ => None,
            }
        })
        .as_ref()
}

unsafe fn make_render(dev: &ID3D11Device) -> Option<Render> {
    let (vs, ps) = shader_bytes()?;
    build_render(dev, vs, ps)
}

/// 悬浮条矩形（显示器像素）→ NDC 四角，按后备缓冲/显示器比例缩放
#[allow(clippy::too_many_arguments)]
fn ndc(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    mon_w: u32,
    mon_h: u32,
    bbw: f32,
    bbh: f32,
) -> (f32, f32, f32, f32) {
    let sx = if mon_w > 0 { bbw / mon_w as f32 } else { 1.0 };
    let sy = if mon_h > 0 { bbh / mon_h as f32 } else { 1.0 };
    let px = x as f32 * sx;
    let py = y as f32 * sy;
    let pw = w as f32 * sx;
    let ph = h as f32 * sy;
    // 像素 → NDC（左上原点 → 中心原点，Y 翻转）
    (
        (px / bbw) * 2.0 - 1.0,
        1.0 - (py / bbh) * 2.0,
        ((px + pw) / bbw) * 2.0 - 1.0,
        1.0 - ((py + ph) / bbh) * 2.0,
    )
}

/// 绑定我们的管线并画一个四边形（两条路径共用；状态存取由调用方负责）
unsafe fn bind_and_draw(
    ctx: &ID3D11DeviceContext,
    r: &Render,
    rtv: &ID3D11RenderTargetView,
    bbw: f32,
    bbh: f32,
    flags: u32,
) {
    let vp = [D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: bbw,
        Height: bbh,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }];
    let rtv_set: [Option<ID3D11RenderTargetView>; 1] = [Some(rtv.clone())];
    ctx.RSSetViewports(Some(&vp));
    ctx.OMSetRenderTargets(Some(&rtv_set), None);
    let blend = if flags & FLAG_STRAIGHT != 0 {
        &r.blend_straight
    } else {
        &r.blend_premul
    };
    ctx.OMSetBlendState(blend, Some(&[0.0, 0.0, 0.0, 0.0]), 0xffff_ffff);
    ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    ctx.IASetInputLayout(&r.layout);
    let stride = std::mem::size_of::<Vert>() as u32;
    let vb_set: [Option<ID3D11Buffer>; 1] = [Some(r.vb.clone())];
    let stride_set: [u32; 1] = [stride];
    let off_set: [u32; 1] = [0];
    ctx.IASetVertexBuffers(
        0,
        1,
        Some(vb_set.as_ptr()),
        Some(stride_set.as_ptr()),
        Some(off_set.as_ptr()),
    );
    ctx.VSSetShader(&r.vs, None);
    ctx.PSSetShader(&r.ps, None);
    if let Some(srv) = &r.srv {
        let arr: [Option<ID3D11ShaderResourceView>; 1] = [Some(srv.clone())];
        ctx.PSSetShaderResources(0, Some(&arr));
    }
    let smp_set: [Option<ID3D11SamplerState>; 1] = [Some(r.samp.clone())];
    ctx.PSSetSamplers(0, Some(&smp_set));
    ctx.Draw(4, 0);
}

/// 每次 Present 前调用。任何一步失败都静默降级为「不画」，绝不拦游戏的 Present。
/// 整条绘制路径裹在 SEH（seh.c 的 __try/__except）里：设备中途失效、驱动 bug、
/// 2077 这类 D3D12+光追的边角情况都可能让 D3D 调用抛访问违例——没有这层守护，
/// 异常会直接带走游戏；有了它，最坏结果只是覆盖层永久停画（DRAW_FAILED）。
unsafe fn draw_overlay(this: *mut c_void) {
    unsafe extern "C" fn trampoline(this: *mut c_void) {
        draw_overlay_inner(this)
    }
    let code = egb_guard_seh(trampoline, this);
    if code != 0 {
        // 异常分支只设标志：堆可能已被这次异常破坏，这里再调日志/分配就是二次崩溃。
        // 宿主侧表现为 draws 不再增长、覆盖层静默消失，游戏照常运行。
        DRAW_FAILED.store(true, Ordering::Relaxed);
    }
}

unsafe fn draw_overlay_inner(this: *mut c_void) {
    let p = SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let flags = (*p).flags;
    if flags & FLAG_VISIBLE == 0 {
        dlog_once_lit("宿主没给可见位 → 不画");
        return;
    }
    if DRAW_FAILED.load(Ordering::Relaxed) {
        dlog_once_lit("之前绘制失败已锁死 → 不再尝试");
        return;
    }
    let handle = (*p).tex;
    if handle == 0 {
        dlog_once_lit("宿主还没给共享纹理 → 不画");
        return;
    }
    let (x, y, w, h, mon_w, mon_h) = ((*p).x, (*p).y, (*p).w, (*p).h, (*p).mon_w, (*p).mon_h);
    if w == 0 || h == 0 {
        dlog_once_lit("悬浮条尺寸为 0 → 不画");
        return;
    }

    // 代理链判定：Present 原函数不来自系统 dxgi.dll → 这条链被 OptiScaler 等代理接管，
    // 后备缓冲归代理管，我们再包一层去画会引发堆破坏闪退（星空实测崩在 ntdll）。只计帧不画。
    let (op, op1) = unsafe { orig_for(this) };
    if !unsafe { fn_in_system_dxgi(op) || fn_in_system_dxgi(op1) } {
        dlog_once_lit("Present 原函数不在系统 dxgi → 代理链（OptiScaler）→ 只计帧不绘制");
        return;
    }

    // 借来的交换链包装（值 = 接口指针；用完 forget，见 borrowed() 的注释）
    let sc: IDXGISwapChain = borrowed(this);
    trace!("enter");
    // 后端判定：拿后备缓冲的原始 QI
    let bb11_raw = raw_get_buffer(this, 0, &ID3D11Texture2D::IID);
    trace!(if bb11_raw.is_some() { "bb11-ok" } else { "bb11-err" });
    let (status, path) = match bb11_raw {
        Some(tex_raw) => {
            let dev11_raw = raw_get_device11(tex_raw);
            trace!(if dev11_raw.is_some() { "dev11-ok" } else { "dev11-err" });
            match dev11_raw {
                Some(d) => {
                    let dev11: ID3D11Device = borrowed(d);
                    let st = draw_overlay_11(&sc, &dev11, flags, handle, x, y, w, h, mon_w, mon_h);
                    std::mem::forget(dev11);
                    (st, PATH_D3D11)
                }
                None => {
                    ensure_queue_hook(this);
                    d3d12_draw(this, &sc, flags, handle, x, y, w, h, mon_w, mon_h)
                }
            }
        }
        None => {
            // D3D12：先惰性装上队列钩子（用游戏自己的设备建哑队列拿虚表）。
            // 队列要等下一次 ExecuteCommandLists 才会被认出来，所以这一帧通常还画不了。
            trace!("qhook-before");
            let hooked = ensure_queue_hook(this);
            trace!(if hooked { "qhook-ok" } else { "qhook-fail" });
            d3d12_draw(this, &sc, flags, handle, x, y, w, h, mon_w, mon_h)
        }
    };
    std::mem::forget(sc);
    match status {
        1 => {}
        3 => dlog_once_lit("绘制未成功：共享纹理打不开"),
        4 => dlog_once_lit("绘制未成功：建资源/绘制失败"),
        5 => dlog_once_lit("绘制未成功：缺 d3dcompiler"),
        6 => dlog_once_lit("绘制未成功：还没抓到 D3D12 渲染队列"),
        _ => dlog_once_lit("绘制未成功：其他状态"),
    }
    (*p).path = path;
    (*p).status = status;
    if status == ST_READY {
        (*p).draws = (*p).draws.wrapping_add(1);
        // 设备失效看门狗：画完这一帧后，游戏设备若已 removed/reset，立刻永久停画。
        // CP2077 实测：它的 D3D12 + 光追 + 异步计算会在我们 D3D11On12 首帧画完后
        // 让设备进入异常态；若我们仍每帧往死设备上提交工作，就会把「可恢复的抖动」
        // 放大成卡死闪退（日志里帧率骤降到 1.94 后会话结束）。检测到失效就只计帧不画，
        // 覆盖层静默关闭，游戏照常运行——这是 OptiScaler/MangoApp 同款保命逻辑。
        let sc: IDXGISwapChain = borrowed(this);
        let dead = if path == PATH_D3D11 {
            sc.GetDevice::<ID3D11Device>()
                .map(|d| d.GetDeviceRemovedReason().is_err())
                .unwrap_or(false)
        } else {
            sc.GetDevice::<windows::Win32::Graphics::Direct3D12::ID3D12Device>()
                .map(|d| d.GetDeviceRemovedReason().is_err())
                .unwrap_or(false)
        };
        std::mem::forget(sc);
        if dead {
            DRAW_FAILED.store(true, Ordering::Relaxed);
            dlog_once_lit("游戏设备已失效（removed/reset）→ 永久停止绘制，保游戏不崩");
        }
    } else if status == ST_DRAW_ERR {
        // 真画不动就别每帧重试（建资源会把游戏拖垮）
        DRAW_FAILED.store(true, Ordering::Relaxed);
    }
}

/// 绘制路径一：游戏本身就是 D3D11，直接借它的设备与立即上下文。
/// 借的是别人的上下文，所以绘制前后必须完整保存/还原管线状态。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_overlay_11(
    sc: &IDXGISwapChain,
    dev: &ID3D11Device,
    flags: u32,
    handle: u64,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    mon_w: u32,
    mon_h: u32,
) -> u32 {
    let Ok(ctx) = dev.GetImmediateContext() else {
        return ST_DRAW_ERR;
    };
    trace!("ctx-ok");

    let mut guard = lock(&RENDER);
    if guard.is_none() || guard.as_ref().unwrap().dev_ptr != dev.as_raw() as usize {
        match make_render(dev) {
            Some(r) => *guard = Some(r),
            None => {
                return if shader_bytes().is_none() {
                    ST_NO_HLSL
                } else {
                    ST_DRAW_ERR
                };
            }
        }
    }
    let r = guard.as_mut().unwrap();

    if !ensure_texture(dev, r, handle) {
        return ST_NO_TEX;
    }

    let Ok(bb) = sc.GetBuffer::<ID3D11Texture2D>(0) else {
        return ST_DRAW_ERR;
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    bb.GetDesc(&mut desc);
    let bbw = desc.Width.max(1) as f32;
    let bbh = desc.Height.max(1) as f32;

    let (l, t, rr, b) = ndc(x, y, w, h, mon_w, mon_h, bbw, bbh);

    let sig = (x, y, w, h, desc.Width, desc.Height);
    if r.last_rect != sig {
        let verts = [
            Vert { x: l, y: t, _z: 0.0, u: 0.0, v: 0.0 },
            Vert { x: rr, y: t, _z: 0.0, u: 1.0, v: 0.0 },
            Vert { x: l, y: b, _z: 0.0, u: 0.0, v: 1.0 },
            Vert { x: rr, y: b, _z: 0.0, u: 1.0, v: 1.0 },
        ];
        write_verts(&ctx, r, &verts);
        r.last_rect = sig;
    }

    let mut rtv: Option<ID3D11RenderTargetView> = None;
    let res: ID3D11Resource = bb.cast().unwrap();
    if dev.CreateRenderTargetView(&res, None, Some(&mut rtv)).is_err() {
        return ST_DRAW_ERR;
    }
    let Some(rtv) = rtv else { return ST_DRAW_ERR };

    // ---- 保存游戏状态 ----
    let mut s = Saved {
        rtv: None,
        dsv: None,
        vp_n: 0,
        vp: [D3D11_VIEWPORT::default(); 4],
        blend: None,
        blend_factor: [0.0; 4],
        sample_mask: 0,
        ds_state: None,
        stencil: 0,
        topo: D3D_PRIMITIVE_TOPOLOGY(0),
        layout: None,
        vb: None,
        vb_stride: 0,
        vb_off: 0,
        vs: None,
        ps: None,
        srv: None,
        sampler: None,
    };
    // 数组形式的出参：windows 0.61 的 D3D11 封装用切片而不是 (count, ptr)
    let mut rtv_arr: [Option<ID3D11RenderTargetView>; 1] = [None];
    ctx.OMGetRenderTargets(Some(&mut rtv_arr), Some(&mut s.dsv));
    s.rtv = rtv_arr[0].clone();
    let mut n_vp: u32 = 0;
    ctx.RSGetViewports(&mut n_vp, None);
    if n_vp > 0 && n_vp <= 4 {
        s.vp_n = n_vp;
        ctx.RSGetViewports(&mut s.vp_n, Some(s.vp.as_mut_ptr()));
    }
    let mut bf: [f32; 4] = [0.0; 4];
    ctx.OMGetBlendState(Some(&mut s.blend), Some(&mut bf), Some(&mut s.sample_mask));
    s.blend_factor = bf;
    ctx.OMGetDepthStencilState(Some(&mut s.ds_state), Some(&mut s.stencil));
    s.layout = ctx.IAGetInputLayout().ok();
    s.topo = ctx.IAGetPrimitiveTopology();
    let mut vb_arr: [Option<ID3D11Buffer>; 1] = [None];
    let mut st_arr: [u32; 1] = [0];
    let mut off_arr: [u32; 1] = [0];
    ctx.IAGetVertexBuffers(
        0,
        1,
        Some(vb_arr.as_mut_ptr()),
        Some(st_arr.as_mut_ptr()),
        Some(off_arr.as_mut_ptr()),
    );
    s.vb = vb_arr[0].clone();
    s.vb_stride = st_arr[0];
    s.vb_off = off_arr[0];
    ctx.VSGetShader(&mut s.vs, None, None);
    ctx.PSGetShader(&mut s.ps, None, None);
    let mut srv_arr: [Option<ID3D11ShaderResourceView>; 1] = [None];
    ctx.PSGetShaderResources(0, Some(&mut srv_arr));
    s.srv = srv_arr[0].clone();
    let mut smp_arr: [Option<ID3D11SamplerState>; 1] = [None];
    ctx.PSGetSamplers(0, Some(&mut smp_arr));
    s.sampler = smp_arr[0].clone();

    // ---- 我们的绘制 ----
    bind_and_draw(&ctx, r, &rtv, bbw, bbh, flags);

    // ---- 还原游戏状态 ----
    ctx.IASetInputLayout(s.layout.as_ref());
    ctx.IASetPrimitiveTopology(s.topo);
    let rvb: [Option<ID3D11Buffer>; 1] = [s.vb.clone()];
    let rst: [u32; 1] = [s.vb_stride];
    let roff: [u32; 1] = [s.vb_off];
    ctx.IASetVertexBuffers(0, 1, Some(rvb.as_ptr()), Some(rst.as_ptr()), Some(roff.as_ptr()));
    ctx.VSSetShader(s.vs.as_ref(), None);
    ctx.PSSetShader(s.ps.as_ref(), None);
    let rsrv: [Option<ID3D11ShaderResourceView>; 1] = [s.srv.clone()];
    ctx.PSSetShaderResources(0, Some(&rsrv));
    let rsmp: [Option<ID3D11SamplerState>; 1] = [s.sampler.clone()];
    ctx.PSSetSamplers(0, Some(&rsmp));
    ctx.OMSetBlendState(s.blend.as_ref(), Some(&s.blend_factor), s.sample_mask);
    ctx.OMSetDepthStencilState(s.ds_state.as_ref(), s.stencil);
    if s.vp_n > 0 {
        ctx.RSSetViewports(Some(&s.vp[..s.vp_n as usize]));
    }
    let rrtv: [Option<ID3D11RenderTargetView>; 1] = [s.rtv.clone()];
    ctx.OMSetRenderTargets(Some(&rrtv), s.dsv.as_ref());

    ST_READY
}

/* ============================== 绘制路径二：D3D12（D3D11On12） ==============================
 * D3D12 游戏的后备缓冲是 D3D12 资源，D3D11 管线碰不到。做法：用 D3D11On12 在那个
 * D3D12 设备上建一套 D3D11 设备，把后备缓冲「包装」成一张 D3D11 纹理，之后完全复用
 * 上面那套绘制管线（着色器/混合/采样器/VB 一模一样）——alpha 混合、直通/预乘处理
 * 都不用重写，这也正是 D3D11On12 存在的意义。
 *
 * 两个硬约束（踩过就崩游戏）：
 *  1. D3D11On12CreateDevice 的队列必须是**游戏自己的渲染队列**。用别的队列就得自己跟
 *     游戏队列做跨队列同步，而我们拿不到它的围栏，写后备缓冲必然竞争。交换链不给队列
 *     （实测 GetDevice(ID3D12CommandQueue) = E_NOINTERFACE），所以靠改写
 *     ID3D12CommandQueue::ExecuteCommandLists 抓（见 exec_lists_detour）。
 *  2. 包装对象持有后备缓冲的引用，游戏的 ResizeBuffers 会因为「后备缓冲还有外部引用」
 *     直接失败 → 切分辨率/切全屏/HDR 切换全挂。所以 ResizeBuffers 的 detour 里必须先
 *     release_on12()（见 resize_detour）。 */

struct Render12 {
    d3d11: ID3D11Device,
    ctx: ID3D11DeviceContext,
    on12: ID3D11On12Device,
    /// 缓存键：队列或后备缓冲换了就重建（换队列 = 游戏换了渲染队列）
    queue_ptr: usize,
    bb_ptr: usize,
    wrapped: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    render: Render,
}

static RENDER12: std::sync::LazyLock<std::sync::Mutex<Option<Render12>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// 冲干净我们提交的工作并解绑，再放掉包装资源。
/// 必须在游戏的 ResizeBuffers 之前做完：D3D11 的对象销毁是延迟的，先把管线解绑 +
/// Flush，后备缓冲的外部引用才会真的松开，否则游戏的 ResizeBuffers 直接失败。
unsafe fn drop_render12(r: Render12) {
    r.ctx.ClearState();
    r.ctx.Flush();
}

unsafe fn release_on12() {
    let mut g = lock(&RENDER12);
    if let Some(r) = g.take() {
        drop_render12(r);
    }
}

/// 建 D3D11On12 设备 + 包装后备缓冲 + 复用 D3D11 管线资源
unsafe fn build_render12(
    queue: &ID3D12CommandQueue,
    bb: &ID3D12Resource,
    bb_ptr: usize,
) -> Result<Render12, u32> {
    let mut dev12: Option<ID3D12Device> = None;
    if queue.GetDevice(&mut dev12).is_err() {
        dlog_once_lit("D3D12 队列取设备失败");
        return Err(ST_NO_QUEUE);
    }
    let Some(dev12) = dev12 else {
        return Err(ST_NO_QUEUE);
    };
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let queues: [Option<windows::core::IUnknown>; 1] = [Some(queue.clone().into())];
    let mut d11: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    if D3D11On12CreateDevice(
        &dev12,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT.0,
        Some(&levels),
        Some(&queues),
        0,
        Some(&mut d11),
        Some(&mut ctx),
        None,
    )
    .is_err()
    {
        dlog_once_lit("D3D11On12CreateDevice 失败");
        return Err(ST_DRAW_ERR);
    }
    let (Some(d11), Some(ctx)) = (d11, ctx) else {
        return Err(ST_DRAW_ERR);
    };
    let on12: ID3D11On12Device = match d11.cast() {
        Ok(o) => o,
        Err(_) => return Err(ST_DRAW_ERR),
    };

    // 包装后备缓冲。In/Out 状态都写 PRESENT：Present 前它本来就该是 PRESENT（游戏刚
    // 呈现完/即将呈现），D3D11On12 会在 Acquire/Release 时自己做 Barrier 进出。
    let rflags = D3D11_RESOURCE_FLAGS {
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut wrapped: Option<ID3D11Texture2D> = None;
    if on12
        .CreateWrappedResource(
            bb,
            &rflags,
            D3D12_RESOURCE_STATE_PRESENT,
            D3D12_RESOURCE_STATE_PRESENT,
            &mut wrapped,
        )
        .is_err()
    {
        dlog_once_lit("CreateWrappedResource 失败（后备缓冲）");
        return Err(ST_DRAW_ERR);
    }
    let Some(wrapped) = wrapped else {
        return Err(ST_DRAW_ERR);
    };
    let res: ID3D11Resource = match wrapped.cast() {
        Ok(r) => r,
        Err(_) => return Err(ST_DRAW_ERR),
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    if d11.CreateRenderTargetView(&res, None, Some(&mut rtv)).is_err() {
        return Err(ST_DRAW_ERR);
    }
    let Some(rtv) = rtv else {
        return Err(ST_DRAW_ERR);
    };
    let Some(render) = make_render(&d11) else {
        dlog_once_lit("D3D11On12 设备上建管线失败（着色器/资源）");
        return Err(ST_DRAW_ERR);
    };
    dlog_once_lit("D3D11On12 就绪（后备缓冲已包装）");

    Ok(Render12 {
        d3d11: d11,
        ctx,
        on12,
        queue_ptr: queue.as_raw() as usize,
        bb_ptr,
        wrapped,
        rtv,
        render,
    })
}

/// D3D12 绘制分派：拿到游戏渲染队列才画得出来（D3D11On12 必须用它的队列提交）
#[allow(clippy::too_many_arguments)]
unsafe fn d3d12_draw(
    this: *mut c_void,
    sc: &IDXGISwapChain,
    flags: u32,
    handle: u64,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    mon_w: u32,
    mon_h: u32,
) -> (u32, u32) {
    let _ = this;
    match direct_queue() {
        Some(q) => {
            dlog_once_lit("走 D3D12(D3D11On12) 路径");
            let r = draw_overlay_12(sc, &q, flags, handle, x, y, w, h, mon_w, mon_h);
            std::mem::forget(q);
            (r, PATH_D3D12)
        }
        None => {
            dlog_once_lit("后备缓冲不是 D3D11 纹理，且还没抓到 D3D12 渲染队列 → 只计帧不绘制");
            (ST_NO_QUEUE, PATH_NONE)
        }
    }
}

/// 绘制路径二入口：D3D12 游戏
#[allow(clippy::too_many_arguments)]
unsafe fn draw_overlay_12(
    sc: &IDXGISwapChain,
    queue: &ID3D12CommandQueue,
    flags: u32,
    handle: u64,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    mon_w: u32,
    mon_h: u32,
) -> u32 {
    let Ok(bb) = sc.GetBuffer::<ID3D12Resource>(0) else {
        return ST_NO_TEX;
    };
    let bb_ptr = bb.as_raw() as usize;
    let desc = bb.GetDesc();
    let bbw = desc.Width.max(1) as u32 as f32;
    let bbh = desc.Height.max(1) as f32;

    let mut guard = lock(&RENDER12);
    trace!("d12-lock");
    let stale = match guard.as_ref() {
        Some(r) => r.queue_ptr != queue.as_raw() as usize || r.bb_ptr != bb_ptr,
        None => true,
    };
    if stale {
        if let Some(old) = guard.take() {
            drop_render12(old);
        }
        trace!("d12-build");
        match build_render12(queue, &bb, bb_ptr) {
            Ok(r) => {
                trace!("d12-built");
                *guard = Some(r)
            }
            Err(st) => return st,
        }
    }
    let r = guard.as_mut().unwrap();

    if !ensure_texture(&r.d3d11, &mut r.render, handle) {
        return ST_NO_TEX;
    }
    trace!("d12-tex");

    let (l, t, rr, b) = ndc(x, y, w, h, mon_w, mon_h, bbw, bbh);
    let sig = (x, y, w, h, desc.Width as u32, desc.Height);
    if r.render.last_rect != sig {
        let verts = [
            Vert { x: l, y: t, _z: 0.0, u: 0.0, v: 0.0 },
            Vert { x: rr, y: t, _z: 0.0, u: 1.0, v: 0.0 },
            Vert { x: l, y: b, _z: 0.0, u: 0.0, v: 1.0 },
            Vert { x: rr, y: b, _z: 0.0, u: 1.0, v: 1.0 },
        ];
        write_verts(&r.ctx, &mut r.render, &verts);
        r.render.last_rect = sig;
    }

    // 借用后备缓冲画一帧：Acquire → 画 → Release → Flush（提交到游戏自己的队列，
    // 于是我们的绘制天然排在游戏这一帧的渲染之后、Present 之前）
    let wrapped_res: ID3D11Resource = match r.wrapped.cast() {
        Ok(x) => x,
        Err(_) => return ST_DRAW_ERR,
    };
    r.on12.AcquireWrappedResources(&[Some(wrapped_res.clone())]);
    trace!("d12-acq");
    bind_and_draw(&r.ctx, &r.render, &r.rtv, bbw, bbh, flags);
    trace!("d12-drawn");
    r.on12.ReleaseWrappedResources(&[Some(wrapped_res)]);
    r.ctx.Flush();
    trace!("d12-flushed");

    ST_READY
}

/* ============================== XInput 手柄拦截（直播浏览器手柄控制） ==============================
 * 见 Shared::gp_* 字段注释。做法：**改各模块 IAT** 里 XInputGetState / XInputGetStateEx
 * 的入口指向 detour——不动 xinput 函数本体，没有 inline hook 的前序指令重定位风险；
 * gp_en=0 时 detour 原样转发（对游戏零影响），gp_en=1 时返回宿主合成的状态。
 * 游戏可能同时 import 多个 xinput 版本（1_3 / 1_4 / 9_1_0），每个导出一个 detour，
 * 各存各的原函数地址，互不串台。xinput 晚加载（游戏运行中才 LoadLibrary）由
 * gp_worker 每秒重扫兜底（幂等：已是 detour 的槽位跳过）。 */

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XiGamepad {
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
struct XiState {
    packet: u32,
    gamepad: XiGamepad,
}

type GetStateFn = unsafe extern "system" fn(u32, *mut XiState) -> u32;

static ORIG_GS13: AtomicUsize = AtomicUsize::new(0);
static ORIG_GS14: AtomicUsize = AtomicUsize::new(0);
static ORIG_EX13: AtomicUsize = AtomicUsize::new(0);
static ORIG_EX14: AtomicUsize = AtomicUsize::new(0);
/// 已改写的 IAT 槽位总数（写入 gp_hooked，宿主据此判断拦截桥是否可用）
static GP_PATCHED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const XINPUT_NOT_CONNECTED: u32 = 1167; // ERROR_DEVICE_NOT_CONNECTED

/// 拦截开启：把宿主合成的槽位状态写给游戏
unsafe fn synth_gp(slot: usize, pstate: *mut XiState) -> u32 {
    let p = SHARED.load(Ordering::Relaxed);
    if p.is_null() || slot > 3 {
        return XINPUT_NOT_CONNECTED;
    }
    let s = (*p).gp_slots[slot];
    if s.result == 0 && !pstate.is_null() {
        (*pstate).packet = s.packet;
        (*pstate).gamepad = XiGamepad {
            w_buttons: s.buttons,
            l_trigger: s.lt,
            r_trigger: s.rt,
            thumb_lx: s.lx,
            thumb_ly: s.ly,
            thumb_rx: s.rx,
            thumb_ry: s.ry,
        };
    }
    s.result
}

macro_rules! gp_detour {
    ($name:ident, $orig:ident) => {
        unsafe extern "system" fn $name(user: u32, pstate: *mut XiState) -> u32 {
            let p = SHARED.load(Ordering::Relaxed);
            let en = if p.is_null() { 0 } else { (*p).gp_en };
            if en == 1 {
                // 控制态：返回宿主合成的静止状态
                return synth_gp(user as usize, pstate);
            }
            let o = $orig.load(Ordering::Relaxed);
            if o == 0 {
                return XINPUT_NOT_CONNECTED;
            }
            let f: GetStateFn = std::mem::transmute(o);
            let r = f(user, pstate);
            if en == 2 && r == 0 && !pstate.is_null() {
                // 锁定态：真实状态只抹掉 B 键位（inj 窗口内反向强制置位 = 回注点按）
                let inj = (*p).gp_inj != 0;
                let b = 0x2000u16;
                let g = &mut (*pstate).gamepad;
                g.w_buttons = if inj { g.w_buttons | b } else { g.w_buttons & !b };
            }
            r
        }
    };
}

gp_detour!(detour_gs13, ORIG_GS13);
gp_detour!(detour_gs14, ORIG_GS14);
gp_detour!(detour_ex13, ORIG_EX13);
gp_detour!(detour_ex14, ORIG_EX14);

/* ---- 键盘轮询拦截（补 WH_GETMESSAGE 的盲区） ----
 * GetAsyncKeyState / GetKeyState / GetKeyboardState 不走消息队列，直接读系统键表，
 * 消息钩子拦不住。把游戏进程内这三个入口的 IAT 槽位也改写掉：悬浮条可见
 * （FLAG_VISIBLE）时返回「没有键按下」，收起后原样转发。星空导入了
 * GetAsyncKeyState（dumpbin 证实），这是它键盘泄漏的潜在通道之一。 */

static ORIG_AKS: AtomicUsize = AtomicUsize::new(0);
static ORIG_GKS: AtomicUsize = AtomicUsize::new(0);
static ORIG_KBS: AtomicUsize = AtomicUsize::new(0);

// detour 签名必须与真实 Win32 API 的 ABI 一致（游戏调的是原生约定）：
// GetAsyncKeyState(int)->SHORT / GetKeyState(int)->SHORT / GetKeyboardState(PBYTE)->BOOL
type GetKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyboardStateFn = unsafe extern "system" fn(*mut u8) -> i32;

unsafe extern "system" fn detour_aks(vk: i32) -> i16 {
    if overlay_wants_input_suppressed() {
        return 0;
    }
    let o = ORIG_AKS.load(Ordering::Relaxed);
    if o == 0 {
        return 0;
    }
    let f: GetKeyStateFn = std::mem::transmute(o);
    f(vk)
}

unsafe extern "system" fn detour_gks(vk: i32) -> i16 {
    if overlay_wants_input_suppressed() {
        return 0;
    }
    let o = ORIG_GKS.load(Ordering::Relaxed);
    if o == 0 {
        return 0;
    }
    let f: GetKeyStateFn = std::mem::transmute(o);
    f(vk)
}

unsafe extern "system" fn detour_kbs(p: *mut u8) -> i32 {
    if overlay_wants_input_suppressed() {
        if !p.is_null() {
            std::ptr::write_bytes(p, 0, 256);
        }
        return 1; // 成功，但键表全空
    }
    let o = ORIG_KBS.load(Ordering::Relaxed);
    if o == 0 {
        return 0;
    }
    let f: GetKeyboardStateFn = std::mem::transmute(o);
    f(p)
}

/// user32 里三个键盘轮询函数的真实地址（首次调用解析并缓存）。
/// 用于「IAT 槽当前值 == 目标地址」匹配——星空按序号导入，拿不到名字。
fn kb_targets() -> (usize, usize, usize) {
    static CACHE: std::sync::OnceLock<(usize, usize, usize)> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| unsafe {
        let name: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        let m = match GetModuleHandleW(PCWSTR(name.as_ptr())) {
            Ok(m) => m,
            Err(_) => return (0, 0, 0),
        };
        (
            get_export(m, "GetAsyncKeyState", 0),
            get_export(m, "GetKeyState", 0),
            get_export(m, "GetKeyboardState", 0),
        )
    })
}

/// (detour 地址, 存原函数的槽) —— 按「模块版本 × 导出名」唯一确定
fn detour_for(module: &str, export: &str) -> Option<(usize, &'static AtomicUsize)> {
    let gs = export == "XInputGetState";
    let ex = export == "XInputGetStateEx";
    if !gs && !ex {
        return None;
    }
    let v14 = module == "xinput1_4.dll";
    let v13 = module == "xinput1_3.dll";
    let v910 = module == "xinput9_1_0.dll";
    if !(v14 || v13 || v910) {
        return None;
    }
    let (f, slot): (usize, &AtomicUsize) = match (v14, gs) {
        (true, true) => (detour_gs14 as usize, &ORIG_GS14),
        (true, false) => (detour_ex14 as usize, &ORIG_EX14),
        (false, true) => (detour_gs13 as usize, &ORIG_GS13),
        (false, false) => (detour_ex13 as usize, &ORIG_EX13),
    };
    Some((f, slot))
}

/// 把一个 IAT 槽位改指向 detour（已指向则跳过）。返回是否新改写了槽位
unsafe fn patch_iat_slot(slot: *mut usize, orig: usize, detour: usize, keep: &AtomicUsize) -> bool {
    let cur = std::ptr::read_volatile(slot);
    if cur == detour {
        return false; // 已挂钩（重复扫描）
    }
    if cur != orig {
        return false; // 已被别人（第三方覆盖层）改过：不抢，避免 detour 链断裂
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(slot as *mut c_void, std::mem::size_of::<usize>(), PAGE_READWRITE, &mut old)
        .is_err()
    {
        return false;
    }
    std::ptr::write_volatile(slot, detour);
    let mut tmp = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(
        slot as *mut c_void,
        std::mem::size_of::<usize>(),
        old,
        &mut tmp,
    );
    let _ = keep.compare_exchange(0, orig, Ordering::SeqCst, Ordering::Relaxed);
    true
}

/// 扫描一个模块的 IAT，改写匹配的 xinput 入口。返回新改写的槽位数
unsafe fn scan_module_iat(base: *mut u8, hmod: HMODULE) -> u32 {
    if base.is_null() {
        return 0;
    }
    // PE 头校验（只处理 64 位镜像：本 DLL 只可能注入 64 位进程）
    // windows 0.61 把 PE 结构拆在多个模块：DOS/NT 签名与导入描述符在 SystemServices，
    // NT_HEADERS64 / 目录枚举在 Diagnostics::Debug，THUNK_DATA64 在 WindowsProgramming。
    use windows::Win32::System::Diagnostics::Debug as dbg;
    use windows::Win32::System::SystemServices as svc;
    use windows::Win32::System::WindowsProgramming as wp;
    use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE;
    let dos = &*(base as *const svc::IMAGE_DOS_HEADER);
    if dos.e_magic != svc::IMAGE_DOS_SIGNATURE {
        return 0;
    }
    let nt = (base.add(dos.e_lfanew as usize)) as *const dbg::IMAGE_NT_HEADERS64;
    if (*nt).Signature != svc::IMAGE_NT_SIGNATURE {
        return 0;
    }
    if (*nt).FileHeader.Machine != IMAGE_FILE_MACHINE(0x8664) {
        return 0;
    }
    let dir = (*nt).OptionalHeader.DataDirectory[dbg::IMAGE_DIRECTORY_ENTRY_IMPORT.0 as usize];
    if dir.VirtualAddress == 0 || dir.Size == 0 {
        return 0;
    }
    let mut desc = base.add(dir.VirtualAddress as usize) as *const svc::IMAGE_IMPORT_DESCRIPTOR;
    let mut patched = 0u32;
    while (*desc).Name != 0 || (*desc).FirstThunk != 0 {
        let name_ptr = base.add((*desc).Name as usize) as *const u8;
        let module = read_cstr_ascii(name_ptr).to_ascii_lowercase();
        let is_xinput = module.ends_with(".dll") && module.starts_with("xinput");
        let is_user32 = module == "user32.dll";
        if is_xinput || is_user32 {
            let oft = (*desc).Anonymous.OriginalFirstThunk;
            let mut int_t =
                base.add(if oft != 0 { oft } else { (*desc).FirstThunk } as usize)
                    as *const wp::IMAGE_THUNK_DATA64;
            let mut iat_t = base.add((*desc).FirstThunk as usize) as *mut wp::IMAGE_THUNK_DATA64;
            while (*int_t).u1.AddressOfData != 0 {
                // user32 键盘轮询入口：只读 IAT 槽当前值做指针匹配，绝不解析 INT 名字。
                // （user32 常被绑定导入剥离 OriginalFirstThunk，此时 int_t 指向 IAT，
                //   里面是绝对地址而非 RVA，按 RVA 解析会读野指针崩溃——见 0x7c89 崩溃）
                if is_user32 {
                    let cur = std::ptr::read_volatile(iat_t as *const usize);
                    if cur != 0 {
                        let (a, b, c) = kb_targets();
                        let hit = if cur == a {
                            Some((detour_aks as usize, &ORIG_AKS))
                        } else if cur == b {
                            Some((detour_gks as usize, &ORIG_GKS))
                        } else if cur == c {
                            Some((detour_kbs as usize, &ORIG_KBS))
                        } else {
                            None
                        };
                        if let Some((d, k)) = hit {
                            if patch_iat_slot(iat_t as *mut usize, cur, d, k) {
                                patched += 1;
                            }
                        }
                    }
                    int_t = int_t.add(1);
                    iat_t = iat_t.add(1);
                    continue;
                }
                let thunk = (*int_t).u1.AddressOfData;
                let mut export = String::new();
                if thunk & (1u64 << 63) != 0 {
                    // 按序号 import：30=GetState，100=GetStateEx
                    export = match (thunk & 0xFFFF) as u32 {
                        30 => "XInputGetState".into(),
                        100 => "XInputGetStateEx".into(),
                        _ => String::new(),
                    };
                } else {
                    // 按名字 import：IMAGE_IMPORT_BY_NAME { hint: u16, name }
                    let nb = base.add(thunk as usize).add(2) as *const u8;
                    export = read_cstr_ascii(nb).to_string();
                }
                let (detour, keep, orig) = match detour_for(&module, &export) {
                    Some((d, k)) => {
                        // 原函数地址：从 xinput 模块直接取（名字取不到再按序号）
                        let o = get_export(hmod, &export, (thunk & 0xFFFF) as u32);
                        (d, k, o)
                    }
                    None => {
                        int_t = int_t.add(1);
                        iat_t = iat_t.add(1);
                        continue;
                    }
                };
                if orig != 0 {
                    let slot = iat_t as *mut usize;
                    if patch_iat_slot(slot, orig, detour, keep) {
                        patched += 1;
                    }
                }
                int_t = int_t.add(1);
                iat_t = iat_t.add(1);
            }
        }
        desc = desc.add(1);
    }
    patched
}

fn read_cstr_ascii<'a>(p: *const u8) -> &'a str {
    if p.is_null() {
        return "";
    }
    let mut n = 0usize;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
        if n > 512 {
            return "";
        }
    }
    let bytes = unsafe { std::slice::from_raw_parts(p, n) };
    std::str::from_utf8(bytes).unwrap_or("")
}

/// 从 xinput 模块解析导出地址（名字优先，序号兜底）
unsafe fn get_export(hmod: HMODULE, name: &str, ordinal: u32) -> usize {
    use windows::core::PCSTR;
    if !name.is_empty() {
        let mut c = name.as_bytes().to_vec();
        c.push(0);
        if let Some(p) = GetProcAddress(hmod, PCSTR(c.as_ptr())) {
            return p as *const () as usize;
        }
    }
    if ordinal >= 30 && ordinal <= 1000 {
        if let Some(p) = GetProcAddress(hmod, PCSTR(ordinal as *const u8)) {
            return p as *const () as usize;
        }
    }
    0
}

/// 全进程模块快照 → 逐个扫 IAT。返回新改写的槽位数
unsafe fn scan_all_modules_xinput() -> u32 {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
        TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    };
    let snap = match CreateToolhelp32Snapshot(
        TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32,
        GetCurrentProcessId(),
    ) {
        Ok(s) => s,
        Err(_) => {
            match CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId()) {
                Ok(s) => s,
                Err(_) => return 0,
            }
        }
    };
    let mut me = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut total = 0u32;
    if Module32FirstW(snap, &mut me).is_ok() {
        loop {
            total += scan_module_iat(me.modBaseAddr as *mut u8, me.hModule);
            if Module32NextW(snap, &mut me).is_err() {
                break;
            }
        }
    }
    let _ = CloseHandle(snap);
    total
}

/// 每秒同步宿主请求：首次无条件扫一遍（宿主进模式前要读 gp_hooked 判断可用性）；
/// 之后 en 变了或有新槽位就重扫并回 ack；拦截开启期间持续重扫
/// （游戏运行中才 LoadLibrary xinput 的情况）
unsafe fn gp_sync() {
    static FIRST: AtomicBool = AtomicBool::new(true);
    let p = SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let first = FIRST.swap(false, Ordering::Relaxed);
    let en = (*p).gp_en;
    let ack = (*p).gp_ack;
    if first || en != ack || en != 0 {
        let n = scan_all_modules_xinput();
        if first {
            // 首次扫描无条件记一笔（含 0），确认 user32 键盘 IAT 改写到底有没有命中
            dlog!("[gp] 首次扫描：改写 {n} 槽位（含 user32 键盘轮询 + xinput）");
        }
        if n > 0 {
            let t = GP_PATCHED.fetch_add(n, Ordering::SeqCst) + n;
            (*p).gp_hooked = t;
            dlog!("[gp] IAT 改写 {n} 个槽位（累计 {t}），en={en}");
        }
        (*p).gp_ack = en;
    }
}

fn spawn_gp_worker() {
    std::thread::spawn(|| loop {
        unsafe { gp_sync() };
        std::thread::sleep(std::time::Duration::from_millis(1000));
    });
}

/* ============================== 初始化 ============================== */

unsafe fn init_shared_memory() -> bool {
    let pid = GetCurrentProcessId();
    let name: Vec<u16> = format!("Local\\EGB_HOOK_{pid}\0").encode_utf16().collect();
    let Ok(handle) = CreateFileMappingW(
        HANDLE(-1isize as *mut c_void),
        None,
        PAGE_READWRITE,
        0,
        256,
        PCWSTR(name.as_ptr()),
    ) else {
        return false;
    };
    let p = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, 256).Value as *mut Shared;
    if p.is_null() {
        return false;
    }
    (*p).magic = MAGIC;
    (*p).pid = pid;
    (*p).frames = 0;
    (*p).p8 = 0;
    (*p).p22 = 0;
    (*p).hooked = 0;
    (*p).status = ST_BOOT;
    (*p).draws = 0;
    (*p).tex = 0;
    (*p).w = 0;
    (*p).h = 0;
    (*p).flags = 0;
    (*p).seq = 0;
    (*p).err = 0;
    (*p).x = 0;
    (*p).y = 0;
    (*p).mon_w = 0;
    (*p).mon_h = 0;
    (*p).gp_hooked = 0;
    (*p).gp_en = 0;
    (*p).gp_ack = 0;
    (*p).gp_slots = [GpSlot::default(); 4];
    SHARED.store(p, Ordering::Relaxed);
    true
}

unsafe fn init_all() {
    dlog!("[init] egb_hook 初始化开始（pid={}）", std::process::id());
    if !init_shared_memory() {
        dlog!("[init] 共享内存创建失败 → 宿主永远看不到这个进程");
        return;
    }
    let Some(hwnd) = make_dummy_window() else {
        dlog!("[init] 哑窗口创建失败");
        return;
    };
    // 两条哑交换链各改各的虚表：blt（CreateSwapChain）与 flip（CreateSwapChainForHwnd）
    // 是两套不同实现、两张不同的虚表，现代游戏几乎全走 flip
    let n_blt = hook_blt_swapchain(hwnd);
    // flip 捕获重试：游戏刚启动时驱动/设备创建可能有瞬时竞争，失败就隔 400ms 再试
    let mut n_flip = 0u32;
    for attempt in 0..3u32 {
        n_flip = hook_flip_swapchain(hwnd);
        if n_flip > 0 {
            break;
        }
        initlog!("[hook] flip 虚表捕获失败（第 {} 次）", attempt + 1);
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
    // D3D12 队列钩子不在这里装：自建 D3D12 设备在已跑 D3D12 的进程里会失败，
    // 改成第一帧绘制时用「后备缓冲反查游戏设备」的方式惰性安装（见 ensure_queue_hook）
    let n_q = 0u32;
    // 输入挂起钩子（Game Bar 等效）：给本进程所有线程装 WH_GETMESSAGE，
    // 悬浮条可见时把游戏的键盘/鼠标/raw input 消息改成 WM_NULL（见文件末尾注释）
    install_input_hook();
    let p = SHARED.load(Ordering::Relaxed);
    if !p.is_null() {
        (*p).hook_blt = n_blt;
        (*p).hook_flip = n_flip;
        (*p).hook_q = n_q;
        (*p).hooked = n_blt + n_flip;
        if n_blt + n_flip > 0 {
            (*p).status = ST_HOOK_ONLY; // 真正的 ST_READY 要等宿主给纹理并开始绘制
        }
    }
    dlog!("[init] 完成：blt={n_blt} flip={n_flip} 队列钩子={n_q}");
    // 手柄拦截桥：独立于交换链钩子，即使绘制路径没装上也要跑（gp_sync 每秒自查）
    spawn_gp_worker();
}

/* ============================ 输入挂起（Game Bar 等效） ============================
 *
 * 星空这类游戏用 raw input（WM_INPUT）读键盘，无视窗口焦点——所以真实覆盖层窗口
 * 弹出时，玩家在悬浮条里按的 Tab 照样漏进游戏、弹出 Pip-Boy。Xbox Game Bar 靠系统
 * InputSite API 挂起游戏输入，但那套 API 本机没有（实测 inputcore.dll 不存在）。
 *
 * 等效做法：我们已经在游戏进程内，直接给本进程所有线程装 WH_GETMESSAGE 钩子。
 * 宿主把悬浮条显示出来（共享内存 FLAG_VISIBLE 置位）时，把游戏消息队列里的键盘 /
 * 鼠标 / raw input 消息就地改成 WM_NULL（0）——游戏 wndproc 收不到任何输入，
 * 悬浮条收起（FLAG_VISIBLE 清零）后自动恢复。改消息而非吞消息，最稳、不影响其他钩子。
 */

/// 本 DLL 的模块句柄（SetWindowsHookExW 需要），DllMain 里捕获。
static HINST: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// 判断是否是要挂起的输入消息（用数值区间，避免逐个引常量）。
/// 键盘 0x100..=0x109、鼠标 0x200..=0x20E、非客户区鼠标 0xA0..=0xA3、raw input 0xFF。
fn is_input_message(m: u32) -> bool {
    (0x0100..=0x0109).contains(&m) // WM_KEYDOWN..WM_UNICHAR
        || (0x0200..=0x020E).contains(&m) // WM_MOUSEMOVE..WM_XBUTTONDBLCLK
        || (0x00A0..=0x00A3).contains(&m) // WM_NCMOUSEMOVE..WM_NCLBUTTONDBLCLK
        || m == 0x00FF // WM_INPUT（raw input：星空读键盘的真实通道）
}

/// 悬浮条当前是否可见（宿主经共享内存 FLAG_VISIBLE 告知）。
/// 从任意线程读一个 u32，无需原子；SHARED 未就绪时按不可见处理（不挂起）。
fn overlay_wants_input_suppressed() -> bool {
    let p = SHARED.load(Ordering::Relaxed);
    !p.is_null() && unsafe { (*p).flags & FLAG_VISIBLE != 0 }
}

unsafe extern "system" fn input_suppress_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 只处理正常动作；只在消息真正被取出时改写。Windows 允许 PM_REMOVE | PM_NOYIELD
    // （值 3），所以必须按位与判断，不能用 == PM_REMOVE（会漏掉带 NOYIELD 的取出）。
    if code == HC_ACTION as i32
        && (wparam.0 as u32 & PM_REMOVE.0) != 0
        && overlay_wants_input_suppressed()
    {
        let msg = lparam.0 as *mut MSG;
        if !msg.is_null() && is_input_message((*msg).message) {
            (*msg).message = 0; // WM_NULL：游戏 wndproc 收到即忽略
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 给本进程所有线程装钩子（只装没装过的）。WH_GETMESSAGE 是同步钩子，
/// 同进程内 SetWindowsHookExW(hmod=本 DLL, tid) 直接把钩子函数挂到目标线程，无需注入。
unsafe fn scan_and_install(hooked: &mut std::collections::HashSet<u32>) {
    let me = GetCurrentProcessId();
    let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut te = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    if Thread32First(snap, &mut te).is_ok() {
        loop {
            if te.th32OwnerProcessID == me && !hooked.contains(&te.th32ThreadID) {
                let proc: HOOKPROC = Some(input_suppress_proc);
                let hinst = HINSTANCE(HINST.load(Ordering::Relaxed));
                if SetWindowsHookExW(WH_GETMESSAGE, proc, Some(hinst), te.th32ThreadID).is_ok() {
                    hooked.insert(te.th32ThreadID);
                }
            }
            if Thread32Next(snap, &mut te).is_err() {
                break;
            }
        }
    }
    let _ = windows::Win32::Foundation::CloseHandle(snap);
}

/// 在 init_all 里调用：先给现有线程装一遍，再起一个后台线程持续补装新线程。
/// 游戏加载期会陆续创建输入/渲染线程，只装启动时那一批会漏；补装循环跑约 60 秒
/// （足够覆盖进主菜单），之后停掉——那时线程集合已稳定。
unsafe fn install_input_hook() {
    let mut hooked = std::collections::HashSet::new();
    scan_and_install(&mut hooked);
    std::thread::spawn(move || {
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(2000));
            unsafe { scan_and_install(&mut hooked) };
        }
    });
    dlog_lit(&["[once] 输入挂起钩子已装（WH_GETMESSAGE 全线程）"]);
}

/// DLL_PROCESS_ATTACH = 1。loader lock 内不干活，丢线程延迟初始化
#[no_mangle]
extern "system" fn DllMain(h: *mut c_void, reason: u32, _reserved: *mut u8) -> i32 {
    if reason == 1 {
        HINST.store(h, Ordering::Relaxed);
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            unsafe { init_all() }
        });
    }
    1
}
