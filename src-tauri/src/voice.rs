//! 直播手柄控制 —— 语音链路（Push-to-Talk）。
//!
//! 架构（规格第十六节）：Voice → STT → Command Parser → 结构化命令 → BrowserController。
//! 本模块只负责「录音 + 交给 STT + 把识别文本回抛」，**不**直接执行任何系统命令；
//! 识别文本经 [`crate::live_pad::on_voice_text`] 走规则解析器，映射到有限的浏览器动作。
//!
//! 录音：Windows Core Audio（WASAPI 共享模式，默认采集设备，事件回调拉包）。
//! STT：本地 whisper.cpp —— 由用户在设置里指定 whisper-cli 可执行文件与模型路径，
//! 录音写成临时 16k 单声道 wav 后以子进程调用，读 stdout 作为识别文本。
//! 未配置/缺文件时给出明确错误提示，不静默失败、不崩 Game Bar。
//!
//! 输入隔离在 live_pad 侧完成：Listening 期间 B 键被 ControllerInputRouter 消费，
//! 游戏收不到（见 live_pad::locked_tick / 拦截桥 en=2）。

#![allow(non_snake_case)]

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, EDataFlow, ERole,
    MMDeviceEnumerator, WAVEFORMATEX, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// 一次录音的停止信号 + 结果回传（capture 线程把 PCM 通过 tx 交回 stop 调用方）
struct Session {
    stop: std::sync::Arc<AtomicBool>,
    joiner: Option<std::thread::JoinHandle<()>>,
    rx: Option<mpsc::Receiver<Capture>>,
}

/// 采集结果：PCM + 采样率 + 实际用到的设备名 + 峰值（判断「麦克风有没有信号」）+ 错误原因。
pub struct Capture {
    pcm: Vec<i16>,
    rate: u32,
    device: String,
    peak: i32,
    err: Option<String>,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            pcm: Vec::new(),
            rate: 16000,
            device: String::new(),
            peak: 0,
            err: Some("采集未启动".into()),
        }
    }
}

/// 读取设备友好名（与 audio.rs 同款逻辑，失败返回空串）。
fn device_name(dev: &IMMDevice) -> String {
    let store = match unsafe { dev.OpenPropertyStore(STGM(0)) } {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let pv = match unsafe { store.GetValue(&PKEY_Device_FriendlyName) } {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    unsafe {
        if let Ok(pw) = PropVariantToStringAlloc(&pv) {
            let name = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as _));
            return name;
        }
    }
    String::new()
}

fn session() -> &'static Mutex<Option<Session>> {
    static S: std::sync::OnceLock<Mutex<Option<Session>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/* ------------------------------ 阶段标记 ------------------------------ */
/// 语音链路阶段。`cancel()`（短按丢弃）只在「录音中」才允许清 [`VOICE_BUSY`]：
/// 识别已经在跑时再来一次 cancel（旧实现会被 B 抖动 / 模式切换反复触发）会把
/// 标记清 0，于是识别还没回来、标记却空了 → 下一次按下被当成新会话，状态彻底错乱
/// （表现为「B 还按着，录音却没了」）。
const PHASE_IDLE: u8 = 0;
const PHASE_REC: u8 = 1;
const PHASE_STT: u8 = 2;
static PHASE: AtomicU8 = AtomicU8::new(PHASE_IDLE);

fn set_phase(p: u8) {
    PHASE.store(p, Ordering::SeqCst);
}

fn phase() -> u8 {
    PHASE.load(Ordering::SeqCst)
}

/// 结束一次语音交互（回空闲）：清忙标记 + 阶段标记
fn finish() {
    set_phase(PHASE_IDLE);
    crate::live_pad::VOICE_BUSY.store(0, Ordering::Relaxed);
}

fn emit_state(app: &AppHandle, state: &str, text: &str) {
    let _ = app.emit(
        "voice://state",
        serde_json::json!({ "state": state, "text": text }),
    );
    sync_hud(app, state);
}

/* ============================== 语音 HUD 浮动窗口 ============================== */

/// HUD 窗口：锁定观看时工具条隐藏，语音状态必须仍然可见——独立顶层小窗。
/// 无装饰、点击穿透、不抢焦点；建一次常驻，按状态显隐 + 跟随直播窗口定位。
const HUD_LABEL: &str = "voice-hud";
const HUD_W: f64 = 280.0;
const HUD_H: f64 = 44.0;

fn sync_hud(app: &AppHandle, state: &str) {
    let a = app.clone();
    let st = state.to_string();
    // 建窗/显隐/定位都涉及窗口操作，统一回主线程（非主线程建窗会触发 WebView2 异常）
    let _ = app.run_on_main_thread(move || {
        let win = match a.get_webview_window(HUD_LABEL) {
            Some(w) => w,
            None => {
                if st == "idle" {
                    return;
                }
                match tauri::WebviewWindowBuilder::new(
                    &a,
                    HUD_LABEL,
                    tauri::WebviewUrl::App("voice-hud.html".into()),
                )
                .title("语音")
                .inner_size(HUD_W, HUD_H)
                .resizable(false)
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .focusable(false)
                .visible(false)
                .build()
                {
                    Ok(w) => {
                        // 只设 WS_EX_TRANSPARENT，不带 LAYERED（理由同 FPS 悬浮窗：
                        // 全屏游戏独立翻转时分层窗口会被踢出合成）
                        let _ = crate::overlay::set_click_through(a.clone(), HUD_LABEL.into(), true);
                        w
                    }
                    Err(e) => {
                        log::warn!("[voice] 创建语音 HUD 窗口失败：{e}");
                        return;
                    }
                }
            }
        };
        if st == "idle" {
            if win.is_visible().unwrap_or(false) {
                let _ = win.hide();
            }
            return;
        }
        position_hud(&a, &win);
        let _ = win.show();
    });
}

/// 定位：优先直播窗口上方居中；没有直播窗口则主显示器底部居中。
fn position_hud(app: &AppHandle, win: &tauri::WebviewWindow) {
    let scale = win.scale_factor().unwrap_or(1.0);
    let (sw, sh) = (HUD_W * scale, HUD_H * scale);
    if let Some(lb) = app.get_webview_window("live-browser") {
        if let (Ok(p), Ok(s)) = (lb.outer_position(), lb.outer_size()) {
            let x = p.x as f64 + (s.width as f64 - sw) / 2.0;
            let y = (p.y as f64 - sh - 8.0).max(4.0);
            let _ = win.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32));
            return;
        }
    }
    if let Ok(Some(mon)) = win.current_monitor() {
        let mp = mon.position();
        let ms = mon.size();
        let x = mp.x + ((ms.width as f64 - sw) / 2.0).round() as i32;
        let y = mp.y + (ms.height as f64 - sh - 96.0).max(0.0).round() as i32;
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// whisper-cli 与模型是否已配置且文件存在。返回 (可用, 原因)。
fn whisper_paths(app: &AppHandle) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let dir = app.path().app_data_dir().map_err(|_| "无法定位数据目录".to_string())?;
    let cfg = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|_| "未找到配置文件".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&cfg).map_err(|e| format!("配置解析失败：{e}"))?;
    let s = v.get("settings").ok_or("缺少 settings")?;
    let exe = s
        .get("whisperExe")
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .ok_or("未设置 whisper 可执行文件")?;
    let model = s
        .get("whisperModel")
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .ok_or("未设置 whisper 模型")?;
    let (exe, model) = (std::path::PathBuf::from(exe), std::path::PathBuf::from(model));
    if !exe.exists() {
        return Err(format!("whisper 可执行文件不存在：{}", exe.display()));
    }
    if !model.exists() {
        return Err(format!("whisper 模型不存在：{}", model.display()));
    }
    Ok((exe, model))
}

/// 前端查询语音是否可用（工具条语音按钮态）。
#[tauri::command]
pub fn voice_available(app: AppHandle) -> serde_json::Value {
    match whisper_paths(&app) {
        Ok(_) => serde_json::json!({ "available": true, "reason": "" }),
        Err(e) => serde_json::json!({ "available": false, "reason": e }),
    }
}

/// 按下沿就要录：**录音从 B 按下那一刻开始**，不是从"按住 500ms 之后"开始。
/// 旧实现按住到阈值才开录，前 0.5 秒的话被整段丢掉，用户一松手只剩很短一段
/// → whisper 拿不到完整句子 → 老是「没听清，请重试」。
/// 短按（没到阈值）由 live_pad 调 [`cancel`] 丢弃这段录音；到阈值再 [`promote`]
/// 亮 HUD 进入正式语音输入。采集线程独立跑，不占轮询线程。
pub fn begin(app: AppHandle) {
    if crate::live_pad::VOICE_BUSY.swap(1, Ordering::SeqCst) != 0 {
        return;
    }
    // 先进「录音中」：这样短按的 cancel 才能正常收尾（含下面路径提前 return 的情况）
    set_phase(PHASE_REC);
    // STT 不可用时不在这里报错：短按（例如锁定态把 B 回注给游戏）不该弹语音提示，
    // 留到 promote（用户确实长按了）再报。
    if let Err(e) = whisper_paths(&app) {
        set_err(Some(format!("语音不可用：{e}")));
        return;
    }
    set_err(None);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Capture>();
    let stop2 = stop.clone();
    let joiner = std::thread::spawn(move || {
        let cap = capture_blocking(stop2);
        let _ = tx.send(cap);
    });
    *session().lock().unwrap_or_else(|e| e.into_inner()) = Some(Session {
        stop,
        joiner: Some(joiner),
        rx: Some(rx),
    });
}

/// 长按到阈值：这次按下确实是要说话 → 亮 HUD「正在听…」（录音早已在跑，不用重开）
pub fn promote(app: &AppHandle) {
    if let Some(e) = take_err() {
        finish();
        emit_state(app, "error", &e);
        let a = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            emit_state(&a, "idle", "");
        });
        return;
    }
    emit_state(app, "listening", "正在听…");
}

/// 短按/误触：丢掉这段录音（停采集、不识别、不亮 HUD、不报错）。
/// **识别已经在跑时直接忽略**：那一下只说明 B 抖动或模式切换，不能把正在进行的
/// 识别判成「已取消」，也不能清掉忙标记（见 [`PHASE`] 注释）。
pub fn cancel() {
    if phase() == PHASE_STT {
        log::info!("[voice] 识别进行中，忽略这次取消（B 抖动/模式切换）");
        return;
    }
    let taken = session().lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(s) = taken {
        s.stop.store(true, Ordering::SeqCst);
        // 不 join：采集线程 50ms 内自己退出并释放设备；轮询线程不能在这卡住
    }
    finish();
    set_err(None);
}

/// whisper-cli / 模型未配置时的原因（begin 记、promote 报）
fn set_err(v: Option<String>) {
    static E: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    *E.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()) = v;
}

fn take_err() -> Option<String> {
    static E: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    E.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// 松开 B → 停止录音 → 识别 → 回抛文本。
pub fn stop(app: AppHandle) {
    if crate::live_pad::VOICE_BUSY.load(Ordering::Relaxed) == 0 {
        return;
    }
    // 进「识别中」阶段：此后短按触发来的 cancel 一律忽略（不许打断这次识别）
    set_phase(PHASE_STT);
    let taken = session().lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(mut s) = taken else {
        finish();
        return;
    };
    s.stop.store(true, Ordering::SeqCst);
    // 等采集线程收尾（最长 500ms），拿回 PCM、采样率、设备与峰值
    let cap = if let Some(rx) = s.rx.take() {
        rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default()
    } else {
        Capture::default()
    };
    if let Some(h) = s.joiner.take() {
        let _ = h.join();
    }
    log::info!(
        "[voice] 录音结束：设备「{}」采样率={} 样本={}（{:.2}s）峰值={} err={:?}",
        if cap.device.is_empty() { "?" } else { &cap.device },
        cap.rate,
        cap.pcm.len(),
        cap.pcm.len() as f64 / cap.rate.max(1) as f64,
        cap.peak,
        cap.err
    );

    if let Some(e) = cap.err {
        finish();
        emit_state(&app, "error", &format!("录音失败：{e}"));
        let a = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            emit_state(&a, "idle", "");
        });
        return;
    }
    // 峰值过低 = 麦克风没信号（设备选错 / 静音 / 蓝牙SCO未连上）
    if cap.peak < 300 {
        finish();
        emit_state(
            &app,
            "error",
            &format!("麦克风无信号（{}），请检查系统默认输入设备", if cap.device.is_empty() { "未知设备" } else { &cap.device }),
        );
        let a = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(3000));
            emit_state(&a, "idle", "");
        });
        return;
    }

    let mut pcm = cap.pcm;
    // 太短（<0.25s）= 误触，直接回 idle（不算错误）
    let rate = cap.rate.max(8000);
    if (pcm.len() as f64) / (rate as f64) < 0.25 {
        finish();
        emit_state(&app, "idle", "");
        return;
    }
    // 尾巴补 0.3s 静音：Push-to-Talk 一松手就断，最后一个字常被切掉，whisper 也少了
    // 「句子结束」的静音尾巴（容易吐半句/乱猜）→ 补一段静音能明显改善「没听清」
    pcm.extend(std::iter::repeat(0i16).take((rate / 3) as usize));

    emit_state(&app, "recognizing", "正在识别…");
    let a = app.clone();
    std::thread::spawn(move || {
        match recognize(&a, &pcm, rate) {
            Some(text) => crate::live_pad::on_voice_text(text),
            None => {
                // recognize 内部已 emit 具体错误；这里只负责复位与超时消隐
                let a2 = a.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(2500));
                    emit_state(&a2, "idle", "");
                });
            }
        }
        finish();
    });
}

/* ============================== 识别 ============================== */

/// PCM(单声道 i16) → 临时 wav → whisper-cli → Some(识别文本)；引擎失败 None（已 emit 错误）。
fn recognize(app: &AppHandle, pcm: &[i16], rate: u32) -> Option<String> {
    let (exe, model) = match whisper_paths(app) {
        Ok(p) => p,
        Err(e) => {
            emit_state(app, "error", &format!("语音不可用：{e}"));
            return None;
        }
    };
    let dir = std::env::temp_dir();
    let wav = dir.join(format!("egb_voice_{}.wav", std::process::id()));
    if write_wav(&wav, pcm, rate).is_err() {
        emit_state(app, "error", "录音写入失败");
        return None;
    }
    // -l zh 中文；-nt 不带时间戳；-np 跳过进度打印；stdout 即纯文本
    let out = crate::spawn_no_window(&exe)
        .args([
            "-m",
            &model.to_string_lossy(),
            "-f",
            &wav.to_string_lossy(),
            "-l",
            "zh",
            "-nt",
            "-np",
        ])
        .output();
    let _ = std::fs::remove_file(&wav);
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // 关键诊断：whisper 到底识别成了什么。之前这里完全没日志，
            // 导致「没听清」分不清是「返回空」还是「返回了但没命中关键词」。
            log::info!(
                "[voice] whisper 识别结果（{} 样本 @{}Hz）：「{}」",
                pcm.len(),
                rate,
                if text.is_empty() { "<空>" } else { &text }
            );
            Some(text)
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            log::warn!("[voice] whisper 退出异常：{err}");
            emit_state(app, "error", "识别失败，请重试");
            None
        }
        Err(e) => {
            log::warn!("[voice] 调用 whisper 失败：{e}");
            emit_state(app, "error", "无法启动语音引擎");
            None
        }
    }
}

fn write_wav(path: &std::path::Path, pcm: &[i16], rate: u32) -> std::io::Result<()> {
    let data_len = (pcm.len() * 2) as u32;
    let mut f = std::fs::File::create(path)?;
    let mut hdr: Vec<u8> = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&(36 + data_len).to_le_bytes());
    hdr.extend_from_slice(b"WAVE");
    hdr.extend_from_slice(b"fmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
    hdr.extend_from_slice(&1u16.to_le_bytes()); // mono
    hdr.extend_from_slice(&rate.to_le_bytes());
    hdr.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    hdr.extend_from_slice(&2u16.to_le_bytes()); // block align
    hdr.extend_from_slice(&16u16.to_le_bytes()); // bits
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_len.to_le_bytes());
    f.write_all(&hdr)?;
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    f.write_all(&bytes)?;
    Ok(())
}

/* ============================== WASAPI 采集 ============================== */

/// 阻塞录音直到 stop 置位，返回 [`Capture`]（PCM + 采样率 + 设备名 + 峰值 + 错误）。
fn capture_blocking(stop: std::sync::Arc<AtomicBool>) -> Capture {
    match capture_inner(&stop) {
        Ok(c) => c,
        Err(e) => Capture {
            err: Some(e),
            ..Default::default()
        },
    }
}

fn capture_inner(stop: &std::sync::Arc<AtomicBool>) -> Result<Capture, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        // 采集设备优先 eConsole（大多数麦克风/蓝牙耳麦挂在这个默认值上），
        // 失败再回退 eCommunications（「通信」角色默认设备）。之前只用
        // eCommunications，蓝牙麦只设了「默认设备」而没设「默认通信设备」时
        // 取到的是另一个（或静音的）设备 → 录到空数据、语音永远识别不到。
        let mut dev = en.GetDefaultAudioEndpoint(EDataFlow(1), ERole(0));
        if dev.is_err() {
            dev = en.GetDefaultAudioEndpoint(EDataFlow(1), ERole(1));
        }
        let dev = dev.map_err(|_| "系统没有可用的默认麦克风".to_string())?;
        let device = device_name(&dev);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let pwfx = match client.GetMixFormat() {
            Ok(p) if !p.is_null() => p,
            _ => return Err("无法获取设备混音格式".into()),
        };
        let wf = *pwfx;
        let rate = wf.nSamplesPerSec.max(8000);
        let channels = wf.nChannels.max(1) as usize;
        let is_float = wf.wFormatTag == 3; // WAVE_FORMAT_IEEE_FLOAT
        let bytes_per_sample = (wf.wBitsPerSample / 8).max(1) as usize;
        // 事件驱动：低延迟、无忙等
        let event = match CreateEventW(None, false, false, PCWSTR::null()) {
            Ok(e) => e,
            Err(_) => {
                CoTaskMemFree(Some(pwfx as _));
                return Err("创建采集事件失败".into());
            }
        };
        let hns = 10_000_000 / 50; // 20ms 缓冲周期
        if client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                hns,
                0,
                pwfx as *const WAVEFORMATEX,
                None,
            )
            .is_err()
        {
            CoTaskMemFree(Some(pwfx as _));
            let _ = CloseHandle(event);
            return Err("设备初始化失败（可能被独占占用）".into());
        }
        let _ = client.SetEventHandle(event);
        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(_) => {
                CoTaskMemFree(Some(pwfx as _));
                let _ = CloseHandle(event);
                return Err("无法获取采集服务".into());
            }
        };
        if client.Start().is_err() {
            CoTaskMemFree(Some(pwfx as _));
            let _ = CloseHandle(event);
            return Err("启动采集失败".into());
        }

        let mut out: Vec<i16> = Vec::new();
        let mut peak: i32 = 0;
        while !stop.load(Ordering::Relaxed) {
            let _ = WaitForSingleObject(event, 50);
            while let Ok(n) = capture.GetNextPacketSize() {
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                if !data.is_null() && frames > 0 {
                    let slice = std::slice::from_raw_parts(
                        data,
                        (frames as usize) * channels * bytes_per_sample,
                    );
                    for fi in 0..frames as usize {
                        let mut acc = 0.0f32;
                        for ch in 0..channels {
                            let off = (fi * channels + ch) * bytes_per_sample;
                            acc += sample_to_f32(&slice[off..off + bytes_per_sample], is_float);
                        }
                        let mono = (acc / channels as f32).clamp(-1.0, 1.0);
                        let s = (mono * i16::MAX as f32) as i16;
                        if s.abs() as i32 > peak {
                            peak = s.abs() as i32;
                        }
                        out.push(s);
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
            }
        }
        let _ = client.Stop();
        CoTaskMemFree(Some(pwfx as _));
        let _ = CloseHandle(event);
        Ok(Capture {
            pcm: out,
            rate,
            device,
            peak,
            err: None,
        })
    }
}

fn sample_to_f32(b: &[u8], is_float: bool) -> f32 {
    if is_float && b.len() >= 4 {
        f32::from_le_bytes([b[0], b[1], b[2], b[3]])
    } else if b.len() >= 2 {
        i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32
    } else {
        0.0
    }
}
