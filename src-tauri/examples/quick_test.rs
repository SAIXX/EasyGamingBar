//! 快捷指令能力干跑验证（不做任何实际动作）：
//! - 打印「强制关闭游戏」当前会命中的目标窗口（前台 / Z 序兜底），不发关闭消息
//!
//! 运行：cargo run --example quick_test
use app_lib::quick::{close_game_target, close_game_target_fallback};

fn main() {
    match close_game_target() {
        Some((pid, title)) => println!("前台路径关闭目标：pid={pid} 标题=「{title}」"),
        None => println!("前台路径：未找到可关闭的前台窗口"),
    }
    match close_game_target_fallback() {
        Some((pid, title)) => println!("Z序兜底关闭目标：pid={pid} 标题=「{title}」"),
        None => println!("Z序兜底：未找到可关闭的窗口"),
    }
}
