//! FPS 端到端自测：启动独立的 Edge 全屏窗口（动画页面持续呈现，模拟游戏），
//! 打印前台游戏检测与各进程 FPS。结束后按进程树关闭该 Edge 实例。
//! 运行：cargo run --example perf_fps_test
//! 调试：EGB_ETW_DEBUG=1 时打印收到的事件直方图。
use app_lib::perf;
use std::process::Command;

fn main() {
    let html = std::env::temp_dir().join("egb_fps_anim.html");
    std::fs::write(
        &html,
        "<!doctype html><style>body{margin:0;background:#000;overflow:hidden}\
         .b{position:absolute;width:180px;height:180px;border-radius:50%;animation:s 2.3s linear infinite}</style>\
         <div class=b style='background:#f60;left:5%;top:10%'></div>\
         <div class=b style='background:#06f;left:45%;top:55%;animation-duration:1.7s'></div>\
         <style>@keyframes s{to{transform:translateX(85vw) rotate(720deg)}}</style>",
    )
    .unwrap();
    let url = format!(
        "file:///{}/",
        html.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
    );
    // kiosk 对 http 友好，起一个临时静态服务
    let server = Command::new("python")
        .args(["-m", "http.server", "18111", "--bind", "127.0.0.1", "--directory", std::env::temp_dir().to_string_lossy().as_ref()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let url = "http://127.0.0.1:18111/egb_fps_anim.html".to_string();
    let profile = std::env::temp_dir().join(format!(
        "egb_edge_{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
    ));
    let child = Command::new("C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe")
        .args([
            format!("--kiosk={url}"),
            "--edge-kiosk-type=fullscreen".into(),
            "--no-first-run".into(),
            format!("--user-data-dir={}", profile.display()),
        ])
        .spawn()
        .expect("启动 Edge 失败");
    println!("edge pid={}，等待全屏呈现…", child.id());
    std::thread::sleep(std::time::Duration::from_secs(7));

    println!("== FPS 端到端自测（10s）==");
    perf::start_for_test();
    for i in 0..10 {
        println!("[{:2}s] present_fps={:?}", i + 1, perf::fps_snapshot());
        println!("      display_fps={:?}（flip 上屏，含帧生成帧）", perf::display_fps());
        let ids = perf::debug_event_ids();
        if !ids.is_empty() {
            println!("      dbg_ids={:?}", ids);
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .status();
    if let Some(mut s) = server {
        let _ = s.kill();
    }
    perf::shutdown();
}
