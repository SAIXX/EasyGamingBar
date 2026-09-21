// 只读自测：扫描本机 Steam 已安装游戏，打印 appid / 名称 / 主程序 / 封面。
// 不触碰系统状态；封面仅复制到临时目录（D-020：测试不改系统状态）。
// 运行：cargo run --example steam_scan
fn main() {
    let cache = std::env::temp_dir().join("egb-steam-test");
    let _ = std::fs::create_dir_all(&cache);
    let games = match app_lib::steam::scan_games(&cache) {
        Ok(g) => g,
        Err(e) => {
            println!("扫描失败：{e}");
            return;
        }
    };
    println!("共检测到 {} 个已安装 Steam 游戏", games.len());
    for g in games.iter() {
        println!(
            "  appid={} | {} \n    exe : {}\n    icon: {}",
            g["appid"], g["name"], g["exe"], g["icon"]
        );
    }
}
