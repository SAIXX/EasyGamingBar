//! 最小 ETW 探针：完全复刻 perf.rs::start_etw 的会话参数，统计 8 秒内
//! DxgKrnl 的 Present(0xB8/0xD7) 与 flip(0x74/0x103/0x182) 事件条数。
//! 用来判定「非提权下会话能起但是不是零事件」。不依赖 app_lib。
//! 运行：cargo run --example etw_probe
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME,
    WNODE_FLAG_TRACED_GUID,
};

const DXGK_GUID: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);
const FILTER_IDS: [u16; 5] = [0xB8, 0xD7, 0x74, 0x103, 0x182];
const SESSION_NAME: &str = "EgbEtwProbe";

static COUNTS: std::sync::Mutex<Option<HashMap<(u16, u32), u64>>> =
    std::sync::Mutex::new(None);
static TOTAL: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn cb(record: *mut EVENT_RECORD) {
    let h = &(*record).EventHeader;
    if h.ProviderId != DXGK_GUID {
        return;
    }
    TOTAL.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut m) = COUNTS.lock() {
        if let Some(m) = m.as_mut() {
            *m.entry((h.EventDescriptor.Id, h.ProcessId)).or_insert(0) += 1;
        }
    }
}

fn main() {
    println!("== ETW 探针：会话名 {SESSION_NAME} ==");
    *COUNTS.lock().unwrap() = Some(HashMap::new());
    unsafe {
        // 清残留
        let mut nw: Vec<u16> = SESSION_NAME.encode_utf16().chain(Some(0)).collect();
        let stop_props = vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + nw.len() * 2];
        let _ = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(nw.as_ptr()),
            stop_props.as_ptr() as *mut EVENT_TRACE_PROPERTIES,
            EVENT_TRACE_CONTROL_STOP,
        );

        let mut buf =
            vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + nw.len() * 2];
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*props).Wnode.BufferSize = buf.len() as u32;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).Wnode.ClientContext = 1;
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).FlushTimer = 1;
        (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;

        let mut handle = CONTROLTRACE_HANDLE::default();
        let ret = StartTraceW(&mut handle, PCWSTR(nw.as_ptr()), props);
        println!("StartTraceW = 0x{:08X}（0 = 成功）", ret.0);
        if ret.0 != 0 {
            return;
        }

        let mut filter: Vec<u8> = Vec::with_capacity(4 + FILTER_IDS.len() * 2);
        filter.push(1);
        filter.push(0);
        filter.extend_from_slice(&(FILTER_IDS.len() as u16).to_le_bytes());
        for id in FILTER_IDS {
            filter.extend_from_slice(&id.to_le_bytes());
        }
        let mut desc = EVENT_FILTER_DESCRIPTOR {
            Ptr: filter.as_ptr() as u64,
            Size: filter.len() as u32,
            Type: EVENT_FILTER_TYPE_EVENT_ID,
        };
        let params = ENABLE_TRACE_PARAMETERS {
            Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
            EnableProperty: 0x10,
            ControlFlags: 0,
            SourceId: GUID::zeroed(),
            EnableFilterDesc: &mut desc,
            FilterDescCount: 1,
        };
        let ret2 = EnableTraceEx2(
            handle,
            &DXGK_GUID,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            5,
            0,
            0,
            0,
            Some(&params),
        );
        println!("EnableTraceEx2 = 0x{:08X}（0 = 成功）", ret2.0);

        let mut name_w: Vec<u16> = SESSION_NAME.encode_utf16().chain(Some(0)).collect();
        let mut logfile = EVENT_TRACE_LOGFILEW::default();
        logfile.LoggerName = PWSTR(name_w.as_mut_ptr());
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME;
        logfile.Anonymous2.EventRecordCallback = Some(cb);
        let trace = OpenTraceW(&mut logfile);
        println!("OpenTrace 有效 = {}", trace.Value != u64::MAX);
        if trace.Value == u64::MAX {
            return;
        }

        // 消费线程：ProcessTrace 阻塞直到会话被停
        let th = std::thread::spawn(move || {
            let _ = ProcessTrace(&[trace], None, None);
        });
        // 采集 8 秒（期间请让桌面/浏览器/游戏有画面变化）
        for i in 1..=8 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            println!("  [{i}s] 累计事件 = {}", TOTAL.load(Ordering::Relaxed));
        }
        // 停会话（按会话名），ProcessTrace 随之返回
        let mut sn: Vec<u16> = SESSION_NAME.encode_utf16().chain(Some(0)).collect();
        let mut sbuf =
            vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + sn.len() * 2];
        let sprops = sbuf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*sprops).Wnode.BufferSize = sbuf.len() as u32;
        (*sprops).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let _ = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(sn.as_ptr()),
            sprops,
            EVENT_TRACE_CONTROL_STOP,
        );
        let _ = th.join();
        let _ = CloseTrace(trace);
    }
    let m = COUNTS.lock().unwrap();
    if let Some(m) = m.as_ref() {
        if m.is_empty() {
            println!("结论：8 秒内**一条 DxgKrnl 事件都没有** —— 本进程权限/系统策略下拿不到 Present/flip。");
        } else {
            let mut v: Vec<_> = m.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            println!("== 事件直方图 (eventId, pid) -> 次数 ==");
            for ((id, pid), c) in v.iter().take(20) {
                println!("   0x{id:02X} pid={pid} : {c}");
            }
        }
    }
}

fn null_ptr() -> *const u16 {
    std::ptr::null()
}
