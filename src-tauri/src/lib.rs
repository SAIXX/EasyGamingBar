pub mod audio;
pub mod autostart;
mod commands;
pub mod detect;
pub mod display;
pub mod gamepad;
pub mod glass;
pub mod guide;
pub mod hotkeys;
pub mod live_chrome;
pub mod live_pad;
pub mod voice;
pub mod net;
pub mod overlay;
pub mod overlay_inject;
pub mod perf;
pub mod playtime;
pub mod quick;
pub mod steam;
pub mod capture;
pub mod watchdog;

/// GUI 进程拉起控制台类子进程（netsh / powershell / cmd / PresentMon / powercfg…）
/// 时 Windows 会新建控制台窗口——短的一闪而过，长的（PresentMon 常驻）就是
/// 「关不掉的黑窗」。统一从这里创建，一律加 CREATE_NO_WINDOW。
/// explorer / osk 这类 GUI 程序不走这里（本来就没有控制台）。
pub fn spawn_no_window(cmd: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = std::process::Command::new(cmd);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}


use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WebviewUrl, WebviewWindowBuilder,
};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 覆盖层 WebView2 全局禁用后台/遮挡节流：
    // 锁定的直播窗口被全屏游戏盖住/踢出合成时、工具条与组件面板隐藏时，Chromium
    // 会把页面定时器压到最低 1 次/分钟 —— 直播页 3s 一次的心跳随之断流，工具条的
    // 25s 看门狗把它误判成「渲染进程崩溃」自动重载/重建，锁定状态与播放被毁
    // （「2077 锁住看视频失效」的根因）。覆盖层应用的页面无论可见性都必须全速
    // 运行，这也是 Electron/Discord 等覆盖层软件的标准配置。已存在的值
    // （如开发时的 --remote-debugging-port）原样保留、只做追加。
    //
    // 关键：--disable-gpu-compositing。直播窗口（live-browser）是常驻 always-on-top
    // 的 WebView2，默认走 GPU 合成，会为自己在 DWM 里持有一条 DirectComposition 翻转链。
    // 当独占全屏游戏抢回前台、走独立翻转（independent flip）时，这条 GPU 翻转链会和
    // 游戏的翻转争用同一显示路径——AMD 驱动（amdkmdag）在这种争用下会 TDR / 设备移除，
    // 表现为「切回游戏瞬间显卡驱动崩溃、游戏闪退、桌面卡死」（实测宿主日志：切回那一刻
    // 游戏帧率骤降到 1.95，随后脏重启 Kernel-Power 41）。让覆盖层改用软件合成后，它不再
    // 持有 GPU 翻转链，只把画面作为普通重定向位图交给 DWM 合成，彻底避开与游戏翻转的争用。
    // 注意：这只关掉「页面图层合成」，视频硬解（媒体引擎）仍走 GPU，4K 直播不受影响。
    {
        const ANTI_THROTTLE: &str = "--disable-background-timer-throttling \
            --disable-backgrounding-occluded-windows --disable-renderer-backgrounding \
            --disable-gpu-compositing";
        let cur =
            std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
        // 两个标记分别判断：老用户环境里可能已有节流参数但缺合成参数，需补齐。
        let mut merged = cur.clone();
        if !merged.contains("--disable-backgrounding-occluded-windows") {
            merged = if merged.trim().is_empty() {
                ANTI_THROTTLE.to_string()
            } else {
                format!("{merged} {ANTI_THROTTLE}")
            };
        } else if !merged.contains("--disable-gpu-compositing") {
            merged = format!("{merged} --disable-gpu-compositing");
        }
        if merged != cur {
            std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", merged);
        }
    }
    // 崩溃兜底：panic 时写 crash 日志并尽力停掉 ETW 内核会话（防泄漏）
    {
        let dir = dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("com.easygamingbar.app");
        let _ = std::fs::create_dir_all(&dir);
        std::panic::set_hook(Box::new(move |info| {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let msg = format!("[ts={ts}] panic: {info}\n");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(dir.join("crash.log"))
            {
                use std::io::Write;
                let _ = f.write_all(msg.as_bytes());
            }
            crate::perf::panic_shutdown();
        }));
    }
    tauri::Builder::default()
        // 单实例保护：二次启动时唤起已有实例的悬浮条（必须最先注册）
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(bar) = app.get_webview_window("bar") {
                let before = (
                    bar.is_visible().unwrap_or(false),
                    bar.is_minimized().unwrap_or(false),
                );
                let _ = bar.unminimize();
                let _ = bar.show();
                // 同样要兜底：最小化态下 show() 不还原（Win+D 会把悬浮条一起最小化）
                overlay::force_show(&bar);
                overlay::ensure_onscreen(&bar);
                overlay::focus_window(&bar);
                log::info!(
                    "单实例唤起悬浮条：唤起前 visible={} minimized={} → 唤起后 visible={} minimized={}",
                    before.0,
                    before.1,
                    bar.is_visible().unwrap_or(false),
                    bar.is_minimized().unwrap_or(false)
                );
            }
        }))
        // 日志插件必须最先注册：配置里的窗口在建 app 时就一并创建了，
        // 装在 setup() 里会整个丢掉「建窗 / WebView2 环境初始化」阶段的报错。
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .max_file_size(512_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("easygamingbar".into()),
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // 性能快照每秒广播的事件源
            perf::init(app.handle().clone());
            // 悬浮条定位到所在显示器顶部水平居中（虚拟桌面坐标需含显示器偏移）
            if let Some(bar) = app.get_webview_window("bar") {
                if let Ok(Some(monitor)) = bar.current_monitor() {
                    let mp = monitor.position();
                    let mw = monitor.size().width;
                    let ww = bar.outer_size().map(|s| s.width).unwrap_or(0);
                    let x = mp.x + ((mw.saturating_sub(ww)) / 2) as i32;
                    let _ = bar.set_position(tauri::PhysicalPosition::new(x, mp.y + 8));
                }
            }
            // 全局快捷键（Win+Shift+M/W/B/A/L/K/G）：麦克风、Wi-Fi、蓝牙、飞行模式、直播锁定、虚拟键盘、显示/隐藏界面
            hotkeys::init(app.handle().clone());
            // Xbox 手柄：Xbox 键唤醒/隐藏界面 + A/B/左摇杆导航（gamepad://input 广播）
            gamepad::init(app.handle().clone());
            // 前台跟踪：低频轮询「是谁在前台」，供收起界面时把焦点还给游戏
            overlay::start_foreground_tracker();
            // 系统托盘：常驻入口（显示/隐藏、设置中心、开机自启、退出）
            build_tray(app.handle())?;
            // 看门狗：TDR / 渲染进程崩溃后自动重载空白的 webview
            watchdog::init(app.handle().clone());
            // 游戏内覆盖层（DLL 注入）：开关由 settings.dllInject 控制，默认关，
            // 线程内部每 1s 读一次配置，打开后才向检测到的前台游戏注入
            overlay_inject::init(app.handle().clone());
            // 直播浏览器手柄控制：模式命令 + 页面探针回包监听
            live_pad::init(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::load_config,
            commands::save_config,
            commands::update_config,
            commands::export_config,
            commands::import_config,
            commands::import_skin_image,
            commands::extract_exe_icon,
            commands::launch_process,
            commands::scan_directory,
            detect::smart_scan_games,
            commands::list_power_plans,
            commands::set_power_plan,
            commands::show_virtual_keyboard,
            commands::default_save_dir,
            commands::open_in_explorer,
            commands::open_system_page,
            commands::debug_log,
            glass::set_glass,
            overlay::focus_overlay,
            overlay::restore_focus,
            overlay::set_click_through,
            gamepad::gamepad_set_owner,
            gamepad::set_input_mode,
            overlay::fps_overlay_launch,
            overlay::fps_overlay_stop,
            overlay::set_click_through,
            steam::scan_steam_games,
            steam::steam_playtime,
            steam::fetch_steam_description,
            playtime::track_game_session,
            live_chrome::attach_live_chrome,
            live_chrome::live_browser_open,
            live_chrome::live_browser_nav,
            live_chrome::live_cursor_hidden,
            live_chrome::live_lock_state,
            live_pad::livepad_set_mode,
            live_pad::livepad_status,
            voice::voice_available,
            audio::list_audio_devices,
            audio::set_default_audio_device,
            audio::get_device_volume,
            audio::set_device_volume,
            audio::list_audio_sessions,
            audio::set_session_volume,
            audio::get_mic_muted,
            audio::toggle_mic_mute,
            display::get_display_status,
            display::set_brightness,
            display::set_display_mode,
            display::set_hdr,
            display::hdr_probe,
            display::toggle_hdr_hotkey,
            net::get_radio_status,
            net::set_wifi_enabled,
            net::set_bt_enabled,
            net::set_airplane_mode,
            net::get_wifi_status,
            net::list_wifi_networks,
            net::connect_wifi,
            net::disconnect_wifi,
            hotkeys::get_hotkey_bindings,
            hotkeys::set_ui_hotkey,
            hotkeys::set_action_hotkey,
            open_settings_center_cmd,
            perf::get_perf_status,
            perf::place_fps_overlay,
            overlay_inject::get_inject_status,
            quick::quick_screenshot,
            quick::quick_screenshot_full,
            quick::quick_open_screenshot_dir,
            quick::quick_show_desktop,
            quick::quick_shutdown,
            quick::quick_close_game,
            guide::guide_run,
            guide::guide_check,
            autostart::get_autostart,
            autostart::set_autostart,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| match event {
            // 内核 ETW 会话不随进程退出自动销毁，需显式停止
            tauri::RunEvent::Exit => {
                perf::shutdown();
            }
            // 托盘常驻：所有窗口都关闭时不退出；code=None 表示非显式退出请求
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            _ => {}
        });

/// 托盘菜单里的「开机自启」勾选项，供事件回调更新勾选状态
static AUTOSTART_ITEM: std::sync::Mutex<Option<CheckMenuItem<tauri::Wry>>> =
    std::sync::Mutex::new(None);

/// 托盘（左键 / 菜单项）的显示隐藏：统一丢回主线程执行。
/// 托盘事件可能来自托盘自己的线程，而窗口操作必须在主线程——否则表现就是
/// 「点了没反应」（日志里 shows 都打了，窗口却没出来）。
fn toggle_ui_on_main(app: &tauri::AppHandle) {
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        // 用户点了托盘 = 明确要操作界面：焦点一并拿到（游戏全屏时同样生效）
        hotkeys::toggle_ui_visibility_with_focus(&handle, true);
    }) {
        log::warn!("调度显示/隐藏失败：{e}");
        hotkeys::toggle_ui_visibility_with_focus(app, true);
    }
}

/// 构建系统托盘与菜单；左键单击托盘 = 显示/隐藏全部界面
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle-ui", "显示 / 隐藏界面", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "open-settings", "设置中心", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启",
        true,
        autostart::get_autostart(),
        None::<&str>,
    )?;
    if let Ok(mut slot) = AUTOSTART_ITEM.lock() {
        *slot = Some(autostart.clone());
    }
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &autostart, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("EasyGamingBar")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().0.as_str() {
            "toggle-ui" => toggle_ui_on_main(app),
            // 窗口创建不能在事件循环线程内联执行（重入会卡死事件循环），放独立线程调度
            "open-settings" => {
                let handle = app.clone();
                std::thread::spawn(move || open_settings_center(&handle));
            }
            "autostart" => toggle_autostart(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_ui_on_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn open_settings_center(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    // 窗口此前被关闭过则重建
    match WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("设置中心")
        .inner_size(880.0, 600.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .center()
        .disable_drag_drop_handler()
        .build()
    {
        Ok(win) => {
            let _ = win.show();
            let _ = win.set_focus();
        }
        Err(e) => log::warn!("打开设置中心失败：{e}"),
    }
}

/// 「更多设置」/悬浮条设置按钮的入口：show/创建窗口必须离开调用方事件循环线程
/// （内联执行会与窗口线程互等卡死，托盘路径同款处理）
#[tauri::command]
pub fn open_settings_center_cmd(app: tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || open_settings_center(&handle));
}

fn toggle_autostart(_app: &tauri::AppHandle) {
    let enable = !autostart::get_autostart();
    if let Err(e) = autostart::set_autostart(enable) {
        log::warn!("切换开机自启失败：{e}");
        return;
    }
    if let Ok(slot) = AUTOSTART_ITEM.lock() {
        if let Some(item) = slot.as_ref() {
            let _ = item.set_checked(enable);
        }
    }
}
}
