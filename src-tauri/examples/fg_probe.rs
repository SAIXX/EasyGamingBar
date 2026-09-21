//! 最小前台窗口 + hook 共享内存探针（不依赖 app_lib）。
//! 1) 打印前台窗口矩形 / 显示器矩形 / 是否严格铺满（perf.rs::current_game 的判定）
//! 2) 读 Local\EGB_FPS_<pid> 共享内存，间隔 1.5s 读两次，看帧计数是否增长
//! 运行：cargo run --example fg_probe
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    System::{
        Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ},
        Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId},
};

fn main() {
    // 传 pid 参数时只做共享内存探测（前台可能是别的窗口）
    if let Some(arg) = std::env::args().nth(1) {
        if let Ok(pid) = arg.parse::<u32>() {
            unsafe {
                let a = read_frames(pid);
                std::thread::sleep(std::time::Duration::from_millis(1500));
                let b = read_frames(pid);
                println!("pid={pid} 第一次={a:?} 1.5s后={b:?}");
                match (a, b) {
                    (Some(x), Some(y)) => println!(
                        "增量={} ≈ {:.1} fps",
                        y.saturating_sub(x),
                        (y.saturating_sub(x)) as f64 / 1.5
                    ),
                    _ => println!("共享内存不存在（hook 未生效）"),
                }
            }
            return;
        }
    }
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            println!("无前台窗口");
            return;
        }
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        println!(
            "窗口矩形 = ({},{})-({},{})  {}x{}",
            r.left,
            r.top,
            r.right,
            r.bottom,
            r.right - r.left,
            r.bottom - r.top
        );
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            println!(
                "显示器矩形 = ({},{})-({},{})  {}x{}",
                mi.rcMonitor.left,
                mi.rcMonitor.top,
                mi.rcMonitor.right,
                mi.rcMonitor.bottom,
                mi.rcMonitor.right - mi.rcMonitor.left,
                mi.rcMonitor.bottom - mi.rcMonitor.top
            );
            println!(
                "严格铺满（= 被识别为游戏）= {}",
                r.left == mi.rcMonitor.left
                    && r.top == mi.rcMonitor.top
                    && r.right == mi.rcMonitor.right
                    && r.bottom == mi.rcMonitor.bottom
            );
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        println!(
            "pid = {pid}  进程名 = {}",
            process_name(pid).unwrap_or_default()
        );

        // hook 共享内存
        println!("-- 读 Local\\EGB_FPS_{pid} --");
        let a = read_frames(pid);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let b = read_frames(pid);
        println!("第一次 = {a:?}");
        println!("1.5s 后 = {b:?}");
        match (a, b) {
            (Some(x), Some(y)) => {
                println!(
                    "增量 = {}（≈ {:.1} fps）",
                    y.saturating_sub(x),
                    (y.saturating_sub(x)) as f64 / 1.5
                );
            }
            _ => println!("共享内存不存在（hook 未生效 / 未注入）"),
        }
    }
}

unsafe fn read_frames(pid: u32) -> Option<u64> {
    let name: Vec<u16> = format!("Local\\EGB_FPS_{pid}\0").encode_utf16().collect();
    let map = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())).ok()?;
    let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 64);
    let base = view.Value as *const u8;
    if base.is_null() {
        return None;
    }
    let magic = (base as *const u32).read_unaligned();
    let frames = (base.add(8) as *const u64).read_unaligned();
    println!("   magic=0x{magic:08X}");
    Some(frames)
}

unsafe fn process_name(pid: u32) -> Option<String> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        HANDLE(h.0),
        PROCESS_NAME_WIN32,
        PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    let _ = CloseHandle(h);
    if ok.is_ok() {
        let s = String::from_utf16_lossy(&buf[..len as usize]);
        Some(s.rsplit(['\\', '/']).next().unwrap_or(&s).to_string())
    } else {
        None
    }
}
