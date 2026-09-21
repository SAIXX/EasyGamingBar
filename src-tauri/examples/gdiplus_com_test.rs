//! 复现「GDI+ 初始化失败」的根因：windows crate 的 GdiplusStartupInput::default()
//! 是全零结构体（GdiplusVersion=0），GDI+ 只接受版本 1，传 0 必返回
//! UnsupportedGdiplusVersion(17)。与线程 COM 状态无关。
//! 运行：cargo run --example gdiplus_com_test
use windows::Win32::Graphics::GdiPlus::{
    GdiplusShutdown, GdiplusStartup, GdiplusStartupInput,
};

fn try_startup(label: &str, version: u32) {
    let mut token = 0usize;
    let input = GdiplusStartupInput {
        GdiplusVersion: version,
        ..Default::default()
    };
    let st = unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) };
    println!("{label}: GdiplusStartup status = {}（0 为成功）", st.0);
    if st.0 == 0 {
        unsafe { GdiplusShutdown(token) };
    }
}

fn main() {
    println!("default() 的 GdiplusVersion = {}", GdiplusStartupInput::default().GdiplusVersion);
    // 复现：default() 全零 → version=0
    try_startup("version=0（现状）", 0);
    // 修复：GDI+ 只认版本 1
    try_startup("version=1（修复后）", 1);
}
