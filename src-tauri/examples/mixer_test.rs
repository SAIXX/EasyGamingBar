// 临时：验证音量混合器会话枚举（列出应用会话、名称、图标路径、音量）
fn main() {
    match app_lib::audio::list_audio_sessions() {
        Ok(list) => {
            if list.is_empty() {
                println!("当前没有音频会话（可播放一段音乐后重试）");
            }
            for s in &list {
                println!(
                    "pid={:<6} active={:<5} muted={:<5} vol={:>3}%  {}  [{}]",
                    s.pid, s.active, s.muted, s.volume, s.name, s.path
                );
            }
            // 回写每个会话当前音量，验证设置链路（无听感变化）
            for s in &list {
                if let Err(e) =
                    app_lib::audio::set_session_volume(s.pid, Some(s.volume as f64), None)
                {
                    println!("设置 pid={} 失败: {e}", s.pid);
                }
            }
            println!("设置链路 OK（{} 个会话回写完成）", list.len());
        }
        Err(e) => {
            println!("错误: {e}");
            std::process::exit(1);
        }
    }
}
