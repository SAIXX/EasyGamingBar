use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONEAREST};

fn primary() -> HMONITOR {
    unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTONEAREST) }
}

fn main() {
    let monitor = primary();
    let device = app_lib::display::monitor_device_name(monitor).expect("设备名失败");
    println!("主显示器 GDI 设备: {device}");

    let modes = app_lib::display::enum_modes(&device).expect("枚举失败");
    println!("可用分辨率 {} 组，前 3 组:", modes.len());
    for m in modes.iter().take(3) {
        println!("  {}×{} @ {:?}", m.width, m.height, m.refreshes);
    }
    let (w, h, hz) = app_lib::display::current_mode(&device).expect("当前模式失败");
    println!("当前模式: {w}×{h}@{hz}Hz");

    // 亮度链路：WMI（内屏）→ DDC/CI（外接）
    // HDR 状态双路探测：CCD AdvancedColorEnabled + DXGI G2084 色彩空间
    let (ccd, dxgi) = app_lib::display::hdr_state_probe(&device);
    println!("HDR 状态 (CCD): {:?}  (DXGI 色彩空间): {:?}", ccd, dxgi);

    let via_wmi = app_lib::display::wmi_get_brightness().unwrap_or(None);
    println!("WMI 亮度: {:?}（None=非内屏或不支持）", via_wmi);
    let via_ddc = app_lib::display::ddc_brightness(monitor, None).unwrap_or(None);
    println!("DDC/CI 亮度: {:?}（None=显示器不支持）", via_ddc);

    let current = via_wmi.or(via_ddc);
    match current {
        Some(b) => {
            println!("设置亮度 50 …");
            let ok = app_lib::display::wmi_set_brightness(50).unwrap_or(false)
                || app_lib::display::ddc_brightness(monitor, Some(50))
                    .unwrap_or(None)
                    .is_some();
            std::thread::sleep(std::time::Duration::from_millis(600));
            let after = app_lib::display::wmi_get_brightness()
                .unwrap_or(None)
                .or(app_lib::display::ddc_brightness(monitor, None).unwrap_or(None));
            println!("设置结果: ok={ok} 读回={after:?}");
            if let Some(orig) = b.checked_sub(50).map(|_| b) {
                let _ = app_lib::display::wmi_set_brightness(orig);
                let _ = app_lib::display::ddc_brightness(monitor, Some(orig));
                println!("已恢复原亮度 {orig}");
            }
        }
        None => println!("此显示器不支持软件调光（电视/HDMI 常见），UI 会显示“不支持”"),
    }
}
