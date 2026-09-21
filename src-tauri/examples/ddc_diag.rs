//! DDC/CI 诊断：枚举显示器 → 物理句柄 → caps 字符串 → VCP 亮度读/写
//! 用法：cargo run --example ddc_diag [-- --set 50]
#![allow(non_snake_case, static_mut_refs)]

use std::io::Write as _;
use windows::Win32::Devices::Display::{
    CapabilitiesRequestAndCapabilitiesReply, DestroyPhysicalMonitor,
    GetCapabilitiesStringLength, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    MC_VCP_CODE_TYPE, PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO,
    MONITORINFOEXW,
};
use windows::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};

static mut MONITORS: Vec<isize> = Vec::new();

unsafe extern "system" fn enum_cb(
    hmon: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rect: *mut RECT,
    _data: LPARAM,
) -> windows::core::BOOL {
    unsafe {
        MONITORS.push(hmon.0 as isize);
    }
    true.into()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let set_to: Option<u32> = args
        .iter()
        .position(|a| a == "--set")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    let gamma_to: Option<u32> = args
        .iter()
        .position(|a| a == "--gamma")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());

    unsafe {
        EnumDisplayMonitors(None, None, Some(enum_cb), LPARAM(0));
        let monitors = std::mem::take(&mut MONITORS);
        println!("检测到 {} 个显示适配器", monitors.len());
        for m in monitors {
            let hmon = HMONITOR(m as *mut _);
            let mut info = MONITORINFOEXW {
                monitorInfo: MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                    ..Default::default()
                },
                szDevice: [0; 32],
            };
            let ok = GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO);
            let device = if ok.as_bool() {
                let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(32);
                String::from_utf16_lossy(&info.szDevice[..len])
            } else {
                "?".into()
            };
            println!("\n=== {device} (HMONITOR {m:#x}) ===");

            let mut count = 0u32;
            let cnt_ok = GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count);
            println!("GetNumberOfPhysicalMonitors: ok={} count={count}", cnt_ok.is_ok());
            if cnt_ok.is_err() || count == 0 {
                continue;
            }
            let mut arr = vec![PHYSICAL_MONITOR::default(); count as usize];
            let got = GetPhysicalMonitorsFromHMONITOR(hmon, &mut arr);
            println!("GetPhysicalMonitors: ok={}", got.is_ok());
            if got.is_err() {
                continue;
            }

            for (i, pm) in arr.iter().enumerate() {
                let handle = pm.hPhysicalMonitor;
                let desc_buf: [u16; 128] = pm.szPhysicalMonitorDescription;
                let len = desc_buf.iter().position(|&c| c == 0).unwrap_or(128);
                let desc = String::from_utf16_lossy(&desc_buf[..len]);
                println!("[{i}] 描述: {desc}");

                // caps 字符串：显示器自报支持的 VCP 码
                let mut clen = 0u32;
                let l_ok = GetCapabilitiesStringLength(handle, &mut clen);
                if l_ok == 0 && clen > 0 && clen < 65536 {
                    let mut buf = vec![0u8; clen as usize];
                    let c_ok = CapabilitiesRequestAndCapabilitiesReply(handle, &mut buf);
                    let caps = String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string();
                    println!("    caps(ok={}): {caps}", c_ok == 0);
                    println!("    caps 含 VCP 10(亮度): {}", check_vcp_in_caps(&caps, "10"));
                } else {
                    println!("    caps: 不可用 (ret={l_ok} len={clen})");
                }
                let _ = std::io::stdout().flush();

                // 读 VCP 0x10（亮度）
                let mut vt = MC_VCP_CODE_TYPE(0);
                let (mut cur, mut max) = (0u32, 0u32);
                let r = GetVCPFeatureAndVCPFeatureReply(
                    handle,
                    0x10,
                    Some(&mut vt),
                    &mut cur,
                    Some(&mut max),
                );
                println!("    读 VCP 0x10: ret={r} cur={cur} max={max} (非0=成功)");
                let _ = std::io::stdout().flush();

                // 写 VCP 0x10（可选，--set N）
                if let Some(v) = set_to {
                    let target = if r != 0 && max > 0 {
                        (v as u64 * max as u64 / 100).min(max as u64) as u32
                    } else {
                        v
                    };
                    let s = SetVCPFeature(handle, 0x10, target);
                    println!("    写 VCP 0x10={target}: ret={s} (0=成功)");
                }

                let _ = std::io::stdout().flush();
            }

            for pm in &arr {
                let _ = DestroyPhysicalMonitor(pm.hPhysicalMonitor);
            }

            // gamma 软件调光测试：--gamma N → 压暗 1.5 秒后恢复
            if let Some(g) = gamma_to {
                let wide: Vec<u16> = device
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let hdc = CreateDCW(
                    windows::core::PCWSTR(wide.as_ptr()),
                    windows::core::PCWSTR::null(),
                    windows::core::PCWSTR::null(),
                    None,
                );
                if hdc.is_invalid() {
                    println!("CreateDC 失败，无法测 gamma");
                    continue;
                }
                let mut orig = [0u16; 768];
                let got = GetDeviceGammaRamp(hdc, orig.as_mut_ptr() as *mut _);
                println!("\ngamma 测试：GetDeviceGammaRamp ok={}", got.as_bool());
                let f = g.clamp(0, 100) as f64 / 100.0;
                let mut ramp = [0u16; 768];
                for i in 0..256usize {
                    let v = ((i as f64 / 255.0) * 65535.0 * f).round().clamp(0.0, 65535.0) as u16;
                    ramp[i] = v;
                    ramp[256 + i] = v;
                    ramp[512 + i] = v;
                }
                let s1 = SetDeviceGammaRamp(hdc, ramp.as_ptr() as *const _);
                println!("gamma 设为 {g}%: ret={} (非0=成功)，1.5 秒后恢复…", s1.as_bool());
                std::thread::sleep(std::time::Duration::from_millis(1500));
                let s2 = if got.as_bool() {
                    SetDeviceGammaRamp(hdc, orig.as_ptr() as *const _)
                } else {
                    // 读不到原值则恢复线性
                    let mut lin = [0u16; 768];
                    for i in 0..256usize {
                        let v = ((i as f64 / 255.0) * 65535.0).round() as u16;
                        lin[i] = v;
                        lin[256 + i] = v;
                        lin[512 + i] = v;
                    }
                    SetDeviceGammaRamp(hdc, lin.as_ptr() as *const _)
                };
                println!("gamma 恢复: ret={} (非0=成功)", s2.as_bool());
                DeleteDC(hdc);
            }
        }
    }
}

fn check_vcp_in_caps(caps: &str, code: &str) -> bool {
    // caps 形如："(prot(monitor)type(LCD)model(...)cmds(1 2 3)vcp(04 10 12 ...))"
    let parts: Vec<&str> = caps.split(&['(', ')'][..]).collect();
    parts.chunks(2).any(|c| {
        c.len() == 2 && c[0].eq_ignore_ascii_case("vcp") && {
            let vals: Vec<&str> = c[1].split_whitespace().collect();
            vals.iter().any(|v| v.split('-').next() == Some(code))
                || vals.iter().any(|v| v.eq_ignore_ascii_case(code))
        }
    })
}
