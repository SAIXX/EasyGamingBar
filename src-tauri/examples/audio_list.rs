// 临时：列出当前音频设备与默认标志（用于测试后恢复默认设备）
// 用法：audio_list                -> 仅列出
//       audio_list <设备名子串>  -> 把该设备设为默认输出+输入角色
fn main() {
    let arg: Option<String> = std::env::args().nth(1);
    if let Some(name) = arg {
        let outs = app_lib::audio::list_devices("output").unwrap();
        if let Some(d) = outs.iter().find(|d| d.name.contains(&name)) {
            app_lib::audio::switch_default(&d.id).expect("切换失败");
            println!("已恢复默认输出: {}", d.name);
        } else {
            println!("未找到包含 \"{name}\" 的输出设备");
        }
        return;
    }
    for kind in ["output", "input"] {
        println!("== {kind} ==");
        match app_lib::audio::list_devices(kind) {
            Ok(devs) => {
                for d in devs {
                    println!("  [{}default] {}
        id={}", if d.is_default { "*" } else { " " }, d.name, d.id);
                }
            }
            Err(e) => println!("  错误: {e}"),
        }
    }
}
