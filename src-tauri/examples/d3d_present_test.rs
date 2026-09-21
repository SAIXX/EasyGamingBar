//! D3D11 全屏呈现模拟游戏：--renderer 模式创建全屏交换链窗口并按 vsync 循环 Present；
//! 默认模式作为父进程轮询 get_perf_status，验证「游戏检测 + Present_Info FPS」链路。
//! 运行：cargo run --example d3d_present_test
use std::process::Command;

fn main() {
    if std::env::args().any(|a| a == "--renderer") {
        render_loop();
        return;
    }

    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .arg("--renderer")
        .spawn()
        .expect("启动渲染进程失败");
    println!("renderer pid={}，等待全屏呈现…", child.id());
    std::thread::sleep(std::time::Duration::from_secs(6));

    println!("== FPS 端到端自测（12s）==");
    for i in 0..12 {
        let status = app_lib::perf::get_perf_status();
        println!("[{:2}s] game={}", i + 1, status["game"]);
        let mut ids = app_lib::perf::debug_event_ids();
        ids.retain(|((pid, _), _)| *pid == child.id());
        ids.sort_by(|a, b| b.1.cmp(&a.1));
        ids.truncate(8);
        ids.sort_by(|a, b| b.1.cmp(&a.1));
        println!("      ids={:?}", ids);
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    let _ = child.kill();
    app_lib::perf::shutdown();
}

fn render_loop() {
    use windows::core::{w, Interface as _};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDeviceAndSwapChain, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_SDK_VERSION, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
        ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGISwapChain, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_FLIP_DISCARD,
        DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
        ShowWindow, TranslateMessage, MSG, PEEK_MESSAGE_REMOVE_TYPE, SW_SHOW, WNDCLASSW,
        WS_POPUP,
    };

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    unsafe {
        let class_name = w!("EGBTestGame");
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // 全屏无边框窗口（覆盖主显示器 → 命中游戏检测）
        let hwnd = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            class_name,
            w!("EGBTestGame"),
            WS_POPUP,
            0,
            0,
            1920,
            1080,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 后台进程默认无法抢前台：模拟 Alt 按键解锁前台切换
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
                KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_MENU,
            };
            let key = |up: bool| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                        ..Default::default()
                    },
                },
            };
            let _ = SendInput(&[key(false), key(true)], std::mem::size_of::<INPUT>() as i32);
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            // 置顶：模拟无边框游戏窗口，并保持 DWM 合成（独立翻转会让置顶窗口不显示）
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                hwnd,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
                0,
                0,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
            );
        }

        let mut desc = DXGI_SWAP_CHAIN_DESC {
            BufferCount: 2,
            OutputWindow: HWND(hwnd.0),
            Windowed: windows::core::BOOL::from(true), // 无边框全屏（现代游戏标准模式）
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        // Windowed=false 在现代 Windows 上通常会退化为 borderless 全屏，不影响 present 事件
        desc.BufferDesc.Format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
        desc.BufferDesc.Width = 1920;
        desc.BufferDesc.Height = 1080;
        desc.BufferDesc.RefreshRate.Numerator = 60;
        desc.BufferDesc.RefreshRate.Denominator = 1;
        desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        desc.SampleDesc.Count = 1;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut swapchain: Option<IDXGISwapChain> = None;
        let hr = D3D11CreateDeviceAndSwapChain(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&desc),
            Some(&mut swapchain),
            Some(&mut device),
            None,
            Some(&mut context),
        );
        if hr.is_err() || swapchain.is_none() {
            eprintln!("D3D11 初始化失败: {hr:?}");
            return;
        }
        let device = device.unwrap();
        let context = context.unwrap();
        let swapchain = swapchain.unwrap();

        let back: ID3D11Texture2D = swapchain.GetBuffer(0).unwrap();
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device
            .CreateRenderTargetView(&back, None, Some(&mut rtv))
            .unwrap();
        let rtv = rtv.unwrap();

        let mut frame: u32 = 0;
        let start = std::time::Instant::now();
        let mut msg = MSG::default();
        while start.elapsed().as_secs_f64() < 600.0 {
            // 简单泵消息避免窗口被标记为未响应
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PEEK_MESSAGE_REMOVE_TYPE(1)).0 != 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let c = if frame % 2 == 0 { 0.9f32 } else { 0.15f32 };
            context.ClearRenderTargetView(&rtv, &[c, 0.2, 0.4, 1.0]);
            let _ = swapchain.Present(1, DXGI_PRESENT(0)); // vsync 呈现
            frame += 1;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = (WPARAM::default(), HWND::default());
    }
}
