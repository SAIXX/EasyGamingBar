//! 性能监控自测：打印 15 秒的 CPU/GPU/VRAM/RAM 占用、前台游戏检测与各进程 FPS。
//! 运行：cargo run --example perf_test
use app_lib::perf;

fn main() {
    println!("== 性能监控自测（15s）==");
    println!("注意：FPS 测量需要管理员权限；游戏检测需要前台全屏窗口。\n");
    for i in 0..15 {
        let status = perf::get_perf_status();
        println!(
            "[{:2}s] cpu={:?}% gpu={:?}% ram={:?}% vram={:?}/{:?} err={:?} game={}",
            i + 1,
            status["cpu"],
            status["gpu"],
            status["ram"],
            status["vram_used"],
            status["vram_total"],
            status["fps_error"],
            status["game"]
        );
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    perf::shutdown();
}
