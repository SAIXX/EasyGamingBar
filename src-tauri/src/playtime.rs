//! 游戏游玩时长本地统计：
//! 经悬浮条启动游戏后跟踪其进程存活时长——先等进程出现（ToolHelp 快照按 exe
//! 文件名粗筛 + 全路径校验；Steam 协议启动要等启动器拉起真身，故最长等 2 分钟），
//! 存活期间按 5s 轮询累计；每 60s 及进程退出时把本次会话增量经 game://play-progress
//! 广播，前端合并进 config.games[].playSeconds 持久化。

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

const POLL_SECS: u64 = 5; // 存活轮询间隔
const FLUSH_SECS: u64 = 60; // 周期广播间隔（进程退出时立即广播）
const WAIT_PROC_SECS: u64 = 120; // 等游戏进程出现的最长时间

#[derive(Clone, Serialize)]
struct PlayProgress {
    #[serde(rename = "exePath")]
    exe_path: String,
    /// 自上次广播以来的新增秒数
    added_secs: u64,
    /// true = 游戏进程已退出（本会话最后一次广播）
    finished: bool,
}

/// 标记由悬浮条启动的游戏，开始统计时长（立即返回，后台线程跟踪）
#[tauri::command]
pub fn track_game_session(app: AppHandle, exe_path: String) {
    let handle = app.clone();
    std::thread::spawn(move || run_session(&handle, &exe_path));
}

/// 归一化路径比较键：统一斜杠 + 小写
fn norm_path(s: &str) -> String {
    s.trim_matches('"')
        .replace('/', "\\")
        .to_lowercase()
}

/// 在进程快照里找 exe 全路径匹配的进程 pid
fn find_pid(target: &str) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
                // 快照只有文件名没有全路径：先按文件名粗筛，命中再开句柄校验全路径
                let exe_file = exe.rsplit(['\\', '/']).next().unwrap_or("");
                let target_file = target.rsplit(['\\', '/']).next().unwrap_or("");
                if exe_file.eq_ignore_ascii_case(target_file) {
                    if let Ok(h) =
                        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, entry.th32ProcessID)
                    {
                        let mut buf = [0u16; 1024];
                        let mut len = buf.len() as u32;
                        let full = if QueryFullProcessImageNameW(
                            h,
                            PROCESS_NAME_WIN32,
                            PWSTR(buf.as_mut_ptr()),
                            &mut len,
                        )
                        .is_ok()
                        {
                            Some(String::from_utf16_lossy(&buf[..len as usize]))
                        } else {
                            None
                        };
                        let _ = CloseHandle(h);
                        if full.map(|f| norm_path(&f) == norm_path(target)).unwrap_or(false) {
                            let pid = entry.th32ProcessID;
                            let _ = CloseHandle(snap);
                            return Some(pid);
                        }
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    None
}

/// 进程是否仍在运行（句柄未超时未触发 = 未退出）
fn alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let a = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        let _ = CloseHandle(h);
        a
    }
}

fn flush(app: &AppHandle, exe_path: &str, added: &mut u64, finished: bool) {
    if *added == 0 && !finished {
        return;
    }
    let _ = app.emit(
        "game://play-progress",
        PlayProgress {
            exe_path: exe_path.to_string(),
            added_secs: std::mem::replace(added, 0),
            finished,
        },
    );
}

fn run_session(app: &AppHandle, exe_path: &str) {
    // 等游戏进程出现（Steam 协议启动时启动器先拉起，真身可能晚几秒到几十秒）
    let wait_rounds = (WAIT_PROC_SECS / 2) as usize;
    let Some(pid) = (0..wait_rounds)
        .find_map(|_| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            find_pid(exe_path)
        })
    else {
        return;
    };
    log::info!("游戏会话开始（pid {pid}）：{exe_path}");
    let mut added: u64 = 0;
    let mut since_flush: u64 = 0;
    let mut last = std::time::Instant::now();
    while alive(pid) {
        std::thread::sleep(std::time::Duration::from_secs(POLL_SECS));
        let now = std::time::Instant::now();
        let dt = now.duration_since(last).as_secs();
        last = now;
        added += dt;
        since_flush += dt;
        if since_flush >= FLUSH_SECS {
            since_flush = 0;
            flush(app, exe_path, &mut added, false);
        }
    }
    // 游戏退出（正常关闭/崩溃）：把剩余增量广播出去
    flush(app, exe_path, &mut added, true);
    log::info!("游戏会话结束：{exe_path}");
}
