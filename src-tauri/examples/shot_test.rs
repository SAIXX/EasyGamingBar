//! 截图链路端到端验证（真实抓屏 + 真实落盘）：
//! 在裸线程上调用 capture_screen_png（等价 Tauri async 命令的 tokio 工作线程），
//! 复刻 quick_screenshot_full 的调用方式，验证 GDI+ PNG 落盘可用。
//! 运行：cargo run --example shot_test
use std::thread;
use windows::Win32::Graphics::GdiPlus::{GdiplusShutdown, GdiplusStartup, GdiplusStartupInput};

fn main() {
    // 前置确认：GdiplusVersion=1 才能启动 GDI+（default() 全零=版本 0 必失败）
    let mut token = 0usize;
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..Default::default()
    };
    let st = unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) };
    println!("GdiplusStartup(version=1) status = {}（0 为成功）", st.0);
    if st.0 == 0 {
        unsafe { GdiplusShutdown(token) };
    }

    // 端到端：裸线程上走真实抓屏→GDI+→PNG 落盘
    let dir = std::env::temp_dir().join("easgbar_shot_test");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_string_lossy().into_owned();
    thread::spawn(move || match app_lib::quick::capture_screen_png(&dir_str) {
        Ok(name) => {
            let path = std::path::Path::new(&dir_str).join(&name);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("抓屏成功：{}（{size} 字节）", path.display());
            let _ = std::fs::remove_file(&path);
        }
        Err(e) => println!("抓屏失败：{e}"),
    })
    .join()
    .unwrap();
}
