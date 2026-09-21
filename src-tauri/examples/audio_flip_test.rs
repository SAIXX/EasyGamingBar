// 对抗测试：切到 LG 后持续监视，被抢回就重设，观察 12 秒内谁赢
use std::time::{Duration, Instant};
fn main() {
    let lg = "{0.0.0.00000000}.{4023ecfb-dc47-4969-aa9e-04bf75555142}";
    let t0 = Instant::now();
    let mut last = String::new();
    let mut reasserts = 0usize;
    while t0.elapsed() < Duration::from_secs(12) {
        if let Ok(list) = app_lib::audio::list_devices("output") {
            if let Some(d) = list.iter().find(|d| d.is_default) {
                let id = d.id.clone();
                if id != last {
                    println!("[{:>6}ms] default -> {} ({})", t0.elapsed().as_millis(), d.name, if id == lg { "LG" } else { "OTHER" });
                    last = id.clone();
                }
                if id != lg && reasserts < 6 {
                    reasserts += 1;
                    println!("[{:>6}ms] >>> 重设回 LG（第 {reasserts} 次）", t0.elapsed().as_millis());
                    let _ = app_lib::audio::switch_default(lg);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    println!("重设总次数: {reasserts}，最终: {last}");
}
