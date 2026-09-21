//! 全屏捕获：DXGI Desktop Duplication（OBS 同源方案）。
//! 相比 GDI BitBlt：能抓到游戏画面（独占/无边框全屏均可）、不会被 TDR 后的
//! 驱动状态卡死；输出 BGRA 像素，经 GDI+ 编码为 PNG 落盘。
//!
//! HDR 注意：桌面处于 HDR 模式时（2077 开 HDR 实测），复制帧不是 BGRA 而是
//! R16G16B16A16_FLOAT（PQ 编码）或 R10G10B10A2——按 4 字节/像素直接读会得到
//! 全黑图（攻略助手「只搜到游戏名没有任务名」的根因）。这里统一转成 SDR BGRA，
//! 并对全黑帧报错，让调用方回退 GDI 抓屏。

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_MAP_READ,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIResource,
    DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, HMONITOR, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::Interface;

/// 光标所在显示器整屏捕获，返回 (宽, 高, BGRA 像素, 行距)
pub fn capture_cursor_monitor() -> Result<(i32, i32, Vec<u8>, i32), String> {
    let mut pt = POINT::default();
    unsafe {
        GetCursorPos(&mut pt).ok();
    }
    let hmon: HMONITOR = unsafe {
        MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
    };

    // 枚举 DXGI 输出，找到 HMONITOR 匹配的那块屏幕
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| e.to_string())?;
    let mut ai = 0u32;
    let (adapter, output) = 'outer: loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(_) => return Err("未找到匹配的显示输出".into()),
        };
        ai += 1;
        let mut oi = 0u32;
        loop {
            match unsafe { adapter.EnumOutputs(oi) } {
                Ok(out) => {
                    oi += 1;
                    let desc: DXGI_OUTPUT_DESC = unsafe { out.GetDesc() }.map_err(|e| e.to_string())?;
                    if desc.Monitor == hmon {
                        break 'outer (adapter, out);
                    }
                }
                Err(_) => break,
            }
        }
    };

    // D3D11 设备
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let levels = [
        windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1,
        windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0,
    ];
    unsafe {
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            7,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| format!("D3D11 设备创建失败：{e}"))?;
    let device = device.ok_or("D3D11 设备为空")?;
    let context = context.ok_or("D3D11 上下文为空")?;

    // 桌面复制
    let output1: IDXGIOutput1 = output.cast().map_err(|e| e.to_string())?;
    let duplication = unsafe { output1.DuplicateOutput(&device) }.map_err(|e| e.to_string())?;

    // 等待一帧（DuplicateOutput 后首帧通常立即可用；静止画面最多等 800ms）
    let mut frameinfo = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
    loop {
        match unsafe { duplication.AcquireNextFrame(100, &mut frameinfo, &mut resource) } {
            Ok(()) => break,
            Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT => {
                if std::time::Instant::now() > deadline {
                    return Err("屏幕画面超时未更新".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            Err(e) => return Err(format!("AcquireNextFrame 失败：{e}")),
        }
    }

    // 帧纹理 → staging 纹理 → CPU 读取
    let texture: ID3D11Texture2D = resource
        .ok_or("帧资源为空".to_string())?
        .cast()
        .map_err(|e| e.to_string())?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ.0 as u32,
        BindFlags: 0,
        MiscFlags: 0,
        ..desc
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .map_err(|e| format!("创建 staging 纹理失败：{e}"))?;
    let staging = staging.ok_or("staging 纹理为空")?;

    unsafe { context.CopyResource(&staging, &texture) };

    let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|e| format!("Map 失败：{e}"))?;

    let (w, h) = (desc.Width as i32, desc.Height as i32);
    let pitch = mapped.RowPitch as usize;
    let src = mapped.pData as *const u8;
    let mut pixels = Vec::with_capacity((w as usize) * (h as usize) * 4);
    // 按帧的实际像素格式转 BGRA。HDR 桌面下复制帧是 FP16（8 字节/像素）——
    // 若还按 4 字节/像素读，行距错位 + 低位全 0，出来就是一张全黑图。
    match desc.Format {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => {
            for row in 0..h as usize {
                let line = unsafe { std::slice::from_raw_parts(src.add(row * pitch), (w as usize) * 4) };
                pixels.extend_from_slice(line);
            }
        }
        DXGI_FORMAT_R16G16B16A16_FLOAT => {
            // FP16→SDR：先线性化，再乘一个保守的 SDR 增益、钳到 [0,1]、sRGB 编码。
            // 不做色调映射（攻略识别只需要可读，不需要色彩准确）。
            for row in 0..h as usize {
                let line = unsafe { std::slice::from_raw_parts(src.add(row * pitch), (w as usize) * 8) };
                let px: &[u16] =
                    unsafe { std::slice::from_raw_parts(line.as_ptr() as *const u16, (w as usize) * 4) };
                for p in px.chunks_exact(4) {
                    // FP16 内存按 RGBA 排布：p[0]=R p[1]=G p[2]=B；输出 BGRA 字节序
                    let r = f16_to_sdr(p[0]);
                    let g = f16_to_sdr(p[1]);
                    let b = f16_to_sdr(p[2]);
                    pixels.extend_from_slice(&[b, g, r, 255]);
                }
            }
        }
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            for row in 0..h as usize {
                let line = unsafe { std::slice::from_raw_parts(src.add(row * pitch), (w as usize) * 4) };
                let px: &[u32] =
                    unsafe { std::slice::from_raw_parts(line.as_ptr() as *const u32, w as usize) };
                for &v in px {
                    // R10G10B10A2：R=bit31-22, G=21-12, B=11-2, A=1-0
                    let r10 = (v >> 22) & 0x3FF;
                    let g10 = (v >> 12) & 0x3FF;
                    let b10 = (v >> 2) & 0x3FF;
                    pixels.extend_from_slice(&[(b10 >> 2) as u8, (g10 >> 2) as u8, (r10 >> 2) as u8, 255]);
                }
            }
        }
        other => {
            unsafe { context.Unmap(&staging, 0) };
            return Err(format!("不支持的桌面复制像素格式 {:?}", other.0));
        }
    }
    unsafe { context.Unmap(&staging, 0) };

    // 全黑帧自检：驱动/游戏组合偶发让复制帧返回纯黑（DXGI 不报错）。与其把黑图
    // 喂给模型（表现为「识别不到任务」），不如报错让调用方回退 GDI BitBlt。
    if pixels_is_black(&pixels) {
        return Err("桌面复制返回全黑帧（可能 HDR/驱动限制）".into());
    }

    Ok((w, h, pixels, (w * 4) as i32))
}

/// PQ(ST.2084) 编码值 → 8bit sRGB（SDR 近似）。
/// HDR 桌面下复制帧的 FP16 通道值是 0~1 的 PQ 编码（不是线性光），直接当线性值
/// 处理会整幅发灰。先走 ST.2084 EOTF 还原到 nits，再按 SDR 白（约 203nits）归一，
/// 最后套 sRGB OETF。不做精细色调映射——攻略识别只要文字可读。
fn f16_to_sdr(v: u16) -> u8 {
    let pq = f16_to_f32(v).clamp(0.0, 1.0);
    const M1: f32 = 0.159_301_757_8;
    const M2: f32 = 78.843_75;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_562_5;
    const C3: f32 = 18.687_5;
    let e = pq.powf(1.0 / M2);
    let num = (e - C1).max(0.0);
    let den = (C2 - C3 * e).max(1e-6);
    let nits = 10_000.0 * (num / den).powf(1.0 / M1);
    // SDR 白 ≈203 nits 归一到 1.0；高光钳位（无滚降，识别足够）
    let lin = (nits / 203.0).clamp(0.0, 1.0);
    let enc = if lin <= 0.0031308 {
        lin * 12.92
    } else {
        1.055 * lin.powf(1.0 / 2.4) - 0.055
    };
    (enc.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as f32;
    let exp = ((h >> 10) & 0x1F) as i32;
    let frac = (h & 0x3FF) as f32;
    let mag = if exp == 0 {
        frac / 1024.0 * 2f32.powi(-14) // 次正规
    } else if exp == 31 {
        f32::INFINITY
    } else {
        (1.0 + frac / 1024.0) * 2f32.powi(exp - 15)
    };
    if sign == 1.0 {
        -mag
    } else {
        mag
    }
}

/// 帧是否几乎全黑：抽样 4096 像素，全部 RGB 低于 8 判定为黑帧。
fn pixels_is_black(pixels: &[u8]) -> bool {
    let total = pixels.len() / 4;
    if total == 0 {
        return true;
    }
    let step = (total / 4096).max(1);
    let mut i = 0;
    while i < total {
        let b = pixels[i * 4] as u32;
        let g = pixels[i * 4 + 1] as u32;
        let r = pixels[i * 4 + 2] as u32;
        if b > 8 || g > 8 || r > 8 {
            return false;
        }
        i += step;
    }
    true
}
