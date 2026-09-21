//! D3D12 flip 交换链测试程序（覆盖层验证用）。
//!
//! 三件事：
//! 1. 探测 `IDXGISwapChain::GetDevice(ID3D12CommandQueue)` 是否可用——覆盖层在 D3D12
//!    游戏里必须拿到游戏的命令队列（要么从交换链取，要么去钩 ExecuteCommandLists）。
//! 2. 探测 D3D12 能否直接打开宿主用 D3D11 `D3D11_RESOURCE_MISC_SHARED`（legacy 句柄）
//!    建的共享纹理——这决定宿主要不要改共享方式。
//! 3. 探测 flip 模型下 `GetBuffer(0)` 返回的资源指针是否随 Present 轮转——决定覆盖层
//!    要不要按「后备缓冲集合」缓存包装资源。
//!
//! 探测完持续 Present（清屏颜色每帧交替），作为注入验证的目标进程。
//! 运行：cargo run --example d3d12_present_test -- 1600x900

fn main() {
    unsafe { run() }
}

unsafe fn run() {
    use windows::core::{w, Interface};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
        ID3D11Device,
    };
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12CreateDevice,
        ID3D12CommandQueue, ID3D12Device, ID3D12Resource,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory2, IDXGIResource, IDXGISwapChain1, DXGI_PRESENT,
        DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW, ShowWindow,
        TranslateMessage, SetForegroundWindow, MSG, PEEK_MESSAGE_REMOVE_TYPE, SW_SHOW, WNDCLASSW,
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

    let (w, h) = std::env::args()
        .find_map(|a| {
            let (a, b) = a.split_once('x')?;
            Some((a.parse::<i32>().ok()?, b.parse::<i32>().ok()?))
        })
        .unwrap_or((1600, 900));
    // --minimal：跳过全部探测步骤（D3D11 共享纹理、哑交换链…），只留「设备+flip 交换链
    // +按 60fps 清屏 Present」。用来区分「设备被搞坏」是探测步骤的锅还是环境本身的问题。
    let minimal = std::env::args().any(|a| a == "--minimal");
    // --d3d11：改成 D3D11 flip 交换链 + 按 60fps 清屏（验证覆盖层的 D3D11 绘制路径）
    let d3d11_mode = std::env::args().any(|a| a == "--d3d11");
    println!("[probe] minimal={minimal} d3d11={d3d11_mode}");

    let hinstance = GetModuleHandleW(None).expect("GetModuleHandle");
    let class = w!("EGBD12Probe");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        Default::default(),
        class,
        w!("EGBD12Probe"),
        WS_POPUP,
        0,
        0,
        w,
        h,
        None,
        None,
        Some(hinstance.into()),
        None,
    )
    .expect("CreateWindowExW");
    let _ = ShowWindow(hwnd, SW_SHOW);
    // 后台进程默认抢不到前台（Windows 前台锁）：游戏检测只看前台窗口，所以先用
    // ALT 敲一下解锁，再 SetForegroundWindow，模拟「玩家切到游戏」。
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
    }
    let _ = SetForegroundWindow(hwnd);
    let _ = DispatchMessageW(&MSG::default());

    // --d3d11：主窗口留给 D3D11 交换链（一个 HWND 同时只能有一条 flip 交换链）
    if d3d11_mode {
        render_loop_d3d11(hwnd, w, h);
        return;
    }

    // ---- D3D12 设备 + DIRECT 队列 ----
    let mut dev: Option<ID3D12Device> = None;
    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut dev).expect("D3D12CreateDevice");
    let dev = dev.expect("device");
    let queue: ID3D12CommandQueue = dev
        .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })
        .expect("CreateCommandQueue");
    println!("[probe] 测试队列 = {:p}", queue.as_raw());

    // ---- 探测 7：同一进程里再建一个 D3D12 设备（覆盖层 DLL 就是这么做的）----
    if !minimal {
        let mut second: Option<ID3D12Device> = None;
        match D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut second) {
            Ok(()) => println!("[probe7] 第二个 D3D12 设备：创建成功"),
            Err(e) => println!("[probe7] 第二个 D3D12 设备：创建失败 {e:?}"),
        }
    }

    // ---- 宿主侧共享纹理（与 overlay_inject.rs 完全一致的做法）----
    let mut legacy_handle_opt = None;
    if !minimal {
        let mut d11: Option<ID3D11Device> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        Default::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut d11),
        None,
        None,
    )
    .expect("D3D11CreateDevice");
    let d11 = d11.expect("d3d11 device");
    use windows::Win32::Graphics::Direct3D11::D3D11_RESOURCE_MISC_SHARED;
    let mut tex: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> = None;
    d11.CreateTexture2D(
        &D3D11_TEXTURE2D_DESC {
            Width: 64,
            Height: 64,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        },
        None,
        Some(&mut tex),
    )
    .expect("CreateTexture2D");
    let tex = tex.expect("shared tex");
    let legacy_handle = tex
        .cast::<IDXGIResource>()
        .expect("cast IDXGIResource")
        .GetSharedHandle()
        .expect("GetSharedHandle");
    println!("[probe] 宿主 legacy 共享句柄 = {:?}", legacy_handle.0);
        legacy_handle_opt = Some(legacy_handle);
    }
    let legacy_handle = legacy_handle_opt;

    // ---- flip 交换链 ----
    let factory: IDXGIFactory2 = CreateDXGIFactory1().expect("CreateDXGIFactory1");
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w as u32,
        Height: h as u32,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: windows::core::BOOL(0),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 3,
        Scaling: windows::Win32::Graphics::Dxgi::DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
        Flags: 0,
    };
    let sc: IDXGISwapChain1 = factory
        .CreateSwapChainForHwnd(&queue, hwnd, &desc, None, None)
        .expect("CreateSwapChainForHwnd");
    println!("[probe] 交换链已创建 = {:p}", sc.as_raw());

    // ---- 探测 1：能不能从交换链拿到命令队列 ----
    match sc.GetDevice::<ID3D12CommandQueue>() {
        Ok(q) => println!(
            "[probe1] GetDevice(ID3D12CommandQueue) OK raw={:p} 与测试队列同一对象={}",
            q.as_raw(),
            q.as_raw() == queue.as_raw()
        ),
        Err(e) => println!("[probe1] GetDevice(ID3D12CommandQueue) 失败：{e:?}"),
    }

    // ---- 探测 2：D3D12 能否打开 D3D11 legacy 共享句柄 ----
    if !minimal {
        if let Some(lh) = legacy_handle {
            let mut opened: Option<ID3D12Resource> = None;
            match dev.OpenSharedHandle(lh, &mut opened) {
                Ok(()) => println!("[probe2] OpenSharedHandle(legacy) OK raw={:?}", opened.is_some()),
                Err(e) => println!("[probe2] OpenSharedHandle(legacy) 失败：{e:?}"),
            }
        }
    }

    // ---- 探测 3：GetBuffer(0) 指针是否随 Present 轮转 ----
    let mut ptrs: Vec<usize> = Vec::new();
    for _ in 0..6 {
        let bb: ID3D12Resource = sc.GetBuffer(0).expect("GetBuffer");
        ptrs.push(bb.as_raw() as usize);
        let _ = sc.Present(1, DXGI_PRESENT(0));
    }
    println!(
        "[probe3] GetBuffer(0) 指针序列 = {:x?}（稳定=同一地址，轮转=交替）",
        ptrs
    );

    // ---- 探测 8：后备缓冲资源能否反查游戏自己的 D3D12 设备（拿队列钩子用）----
    {
        let bb: ID3D12Resource = sc.GetBuffer(0).expect("GetBuffer");
        let mut d: Option<ID3D12Device> = None;
        match bb.GetDevice(&mut d) {
            Ok(()) => println!(
                "[probe8] 后备缓冲反查设备：OK，是同一个设备={}",
                d.as_ref().map(|x| x.as_raw() == dev.as_raw()).unwrap_or(false)
            ),
            Err(e) => println!("[probe8] 后备缓冲反查设备失败：{e:?}"),
        }
    }

    // ---- 探测 9：D3D12 交换链上调用 GetDevice(ID3D11Device) —— 覆盖层用它区分 D3D11/D3D12 ----
    match sc.GetDevice::<windows::Win32::Graphics::Direct3D11::ID3D11Device>() {
        Ok(_) => println!("[probe9] GetDevice(ID3D11Device) OK（D3D12 交换链竟给了 D3D11 设备）"),
        Err(e) => println!("[probe9] GetDevice(ID3D11Device) 干净失败：{e:?}"),
    }

    // ---- 探测 10：D3D12 交换链上 GetBuffer<ID3D11Texture2D>（覆盖层用它判后端）----
    {
        let r: windows::core::Result<
            windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        > = sc.GetBuffer(0);
        match r {
            Ok(_) => println!("[probe10] GetBuffer<ID3D11Texture2D> OK（D3D12 交换链竟给了 D3D11 纹理）"),
            Err(e) => println!("[probe10] GetBuffer<ID3D11Texture2D> 干净失败：{e:?}"),
        }
    }

    // ---- 探测 4：flip 虚表槽位地址（8=Present / 13=ResizeBuffers / 22=Present1 / 36=ResizeBuffers1）----
    // COM 对象首字段才是虚表指针：`*(对象指针)` = 虚表，槽位 i 的函数地址 = vtbl[i]。
    // （少了这一层解引用就会读到对象内部字段——原 hook 就是栽在这里。）
    unsafe fn vtable_of(obj: *mut std::ffi::c_void) -> *const *const std::ffi::c_void {
        *(obj as *const *const *const std::ffi::c_void)
    }
    let vtbl = vtable_of(sc.as_raw());
    let slot = |i: usize| unsafe { *vtbl.add(i) as usize };
    println!(
        "[probe4] flip 虚表：8={:#x} 13={:#x} 22={:#x} 34={:#x} 35={:#x} 36={:#x} 37={:#x}",
        slot(8),
        slot(13),
        slot(22),
        slot(34),
        slot(35),
        slot(36),
        slot(37)
    );

    // ---- 探测 5/6：复现 DLL 里 flip 哑交换链的两种 AlphaMode ----
    if !minimal {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDeviceAndSwapChain, ID3D11Device,
        };
        use windows::Win32::Graphics::Dxgi::{
            IDXGISwapChain, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
            DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL,
        };
        let probe_hwnd = CreateWindowExW(
            Default::default(),
            class,
            w!("EGBD12ProbeDummy"),
            windows::Win32::UI::WindowsAndMessaging::WS_OVERLAPPED,
            0,
            0,
            8,
            8,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .expect("dummy window");
        let bad_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 8,
            Height: 8,
            ..desc
        };
        let bad_desc = DXGI_SWAP_CHAIN_DESC1 {
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..bad_desc
        };
        match factory.CreateSwapChainForHwnd(
            &queue,
            probe_hwnd,
            &bad_desc,
            None,
            None::<&windows::Win32::Graphics::Dxgi::IDXGIOutput>,
        ) {
            Ok(_) => println!("[probe5] PREMULTIPLIED flip 哑交换链：创建成功（假设被推翻）"),
            Err(e) => println!("[probe5] PREMULTIPLIED flip 哑交换链创建失败：{e:?}"),
        }
        let good_desc = DXGI_SWAP_CHAIN_DESC1 {
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            ..bad_desc
        };
        match factory.CreateSwapChainForHwnd(
            &queue,
            probe_hwnd,
            &good_desc,
            None,
            None::<&windows::Win32::Graphics::Dxgi::IDXGIOutput>,
        ) {
            Ok(sc2) => {
                let v2 = vtable_of(sc2.as_raw());
                let s2 = |i: usize| unsafe { *v2.add(i) as usize };
                println!(
                    "[probe6] UNSPECIFIED flip 哑交换链：创建成功，虚表 8={:#x} 22={:#x}（与游戏交换链相同={}）",
                    s2(8),
                    s2(22),
                    s2(8) == slot(8) && s2(22) == slot(22)
                );
            }
            Err(e) => println!("[probe6] UNSPECIFIED flip 哑交换链创建失败：{e:?}"),
        }
        // blt 哑交换链（DLL 里的另一条路径）：对比虚表
        let blt_desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 8,
                Height: 8,
                RefreshRate: DXGI_RATIONAL {
                    Numerator: 0,
                    Denominator: 0,
                },
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
                Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: probe_hwnd,
            Windowed: windows::core::BOOL(1),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            Flags: 0,
        };
        let mut sc_blt: Option<IDXGISwapChain> = None;
        let mut dev_blt: Option<ID3D11Device> = None;
        let hr = D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&blt_desc),
            Some(&mut sc_blt),
            Some(&mut dev_blt),
            None,
            None,
        );
        match (hr.is_ok(), sc_blt.clone()) {
            (true, Some(sb)) => {
                let v3 = vtable_of(sb.as_raw());
                let s3 = |i: usize| unsafe { *v3.add(i) as usize };
                println!(
                    "[probe6b] blt 哑交换链：虚表 8={:#x}（与 flip 的 Present 相同={}）；22={:#x}",
                    s3(8),
                    s3(8) == slot(8),
                    s3(22)
                );
            }
            _ => println!("[probe6b] blt 哑交换链创建失败：{hr:?}"),
        }
    }

    // ---- 渲染循环：持续 Present，供注入验证 ----
    use windows::Win32::Graphics::Direct3D12::{
        ID3D12CommandAllocator, ID3D12CommandList, ID3D12DescriptorHeap, ID3D12Fence,
        ID3D12GraphicsCommandList,
        D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_DESCRIPTOR_HEAP_DESC, D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
        D3D12_FENCE_FLAG_NONE,
        D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
        D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_TRANSITION_BARRIER,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    // 围栏 + 每帧「等上一帧跑完再 Reset 分配器」：D3D12 的分配器在其命令列表执行完之前
    // 不能 Reset（无围栏就 Reset 会把设备搞成 DEVICE_REMOVED —— 测试程序必须自己做对，
    // 否则测出来的全是自己的错）。
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("CreateFence");
    let evt = CreateEventW(None, false, false, None).expect("CreateEventW");
    let mut fence_value: u64 = 0;
    let alloc: ID3D12CommandAllocator = dev
        .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
        .expect("CreateCommandAllocator");
    let list: ID3D12GraphicsCommandList = dev
        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
        .expect("CreateCommandList");
    let _ = list.Close();

    let heap: ID3D12DescriptorHeap = dev
        .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: 4,
            Flags: Default::default(),
            NodeMask: 0,
        })
        .expect("CreateDescriptorHeap");
    let rtv0 = heap.GetCPUDescriptorHandleForHeapStart();

    // 后备缓冲地址稳定（probe3 已验证），RTV 建一次就够。
    // 每帧往同一块描述符里重写会在 GPU 还在读的时候改描述符——很容易把设备搞成
    // DEVICE_REMOVED，那是测试程序自己的锅，不是被注入的 DLL。
    {
        let bb0: ID3D12Resource = sc.GetBuffer(0).expect("GetBuffer");
        dev.CreateRenderTargetView(&bb0, None, rtv0);
    }

    let mut msg = MSG::default();
    let start = std::time::Instant::now();
    let mut frame: u32 = 0;
    let mut last_report = std::time::Instant::now();
    let mut last_frames: u32 = 0;
    println!("[probe] 开始渲染循环（600s），composite 显示会把覆盖层画在上面");
    while start.elapsed().as_secs_f64() < 600.0 {
        while PeekMessageW(&mut msg, None, 0, 0, PEEK_MESSAGE_REMOVE_TYPE(1)).0 != 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        // 等上一帧的命令列表执行完，才能 Reset 分配器
        if fence.GetCompletedValue() < fence_value {
            let _ = fence.SetEventOnCompletion(fence_value, evt);
            let _ = WaitForSingleObject(evt, 2000);
        }
        let _ = alloc.Reset();
        let _ = list.Reset(&alloc, None);
        let bb: ID3D12Resource = match sc.GetBuffer(0) {
            Ok(b) => b,
            Err(_) => break,
        };
        let rtv = rtv0;
        let b1 = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(bb.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_PRESENT,
                    StateAfter: D3D12_RESOURCE_STATE_RENDER_TARGET,
                }),
            },
        };
        list.ResourceBarrier(&[b1]);
        let c = if frame % 2 == 0 {
            [0.05f32, 0.05, 0.05, 1.0]
        } else {
            [0.1f32, 0.05, 0.2, 1.0]
        };
        list.ClearRenderTargetView(rtv, &c, None);
        let b2 = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(bb.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_RENDER_TARGET,
                    StateAfter: D3D12_RESOURCE_STATE_PRESENT,
                }),
            },
        };
        list.ResourceBarrier(&[b2]);
        let _ = list.Close();
        let lists: [Option<ID3D12CommandList>; 1] = [Some(list.cast().unwrap())];
        queue.ExecuteCommandLists(&lists);
        fence_value += 1;
        let _ = queue.Signal(&fence, fence_value);
        let _ = sc.Present(1, DXGI_PRESENT(0));
        frame += 1;
        // 限帧到 ~60fps：窗口化 flip 交换链在没被节流时会以十万帧/秒空转（Present 立刻
        // 返回），那不是真实游戏负载，还会把 GPU 提交队列堆到 TDR（DEVICE_REMOVED），
        // 反而把被测的 DLL 拖下水。
        std::thread::sleep(std::time::Duration::from_millis(16));
        // 每秒一行：帧率（Present 有没有被节流）+ Present 返回码（DEVICE_REMOVED 之类）
        if last_report.elapsed().as_secs_f64() >= 1.0 {
            let dt = last_report.elapsed().as_secs_f64();
            println!(
                "[probe] t={:.0}s 帧率≈{:.0}/s 累计={} 设备={:?}",
                start.elapsed().as_secs_f64(),
                (frame - last_frames) as f64 / dt,
                frame,
                dev.GetDeviceRemovedReason().map(|_| "正常")
            );
            last_report = std::time::Instant::now();
            last_frames = frame;
        }
    }
    println!("[probe] 渲染循环结束，共 {frame} 帧");
}

/// D3D11 flip 交换链渲染（--d3d11）：覆盖层 DLL 的 D3D11 直画路径需要一个 D3D11 目标。
fn render_loop_d3d11(hwnd: windows::Win32::Foundation::HWND, w: i32, h: i32) {
    use windows::core::{Interface, BOOL};
    use windows::Win32::Foundation::HWND as H;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
        ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1,
        DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PEEK_MESSAGE_REMOVE_TYPE,
    };
    unsafe {
        let mut d: Option<ID3D11Device> = None;
        let mut c: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut d),
            None,
            Some(&mut c),
        )
        .expect("D3D11CreateDevice");
        let dev = d.expect("d3d11 dev");
        let ctx = c.expect("d3d11 ctx");
        let factory: IDXGIFactory2 = CreateDXGIFactory1().expect("factory");
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w as u32,
            Height: h as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: windows::Win32::Graphics::Dxgi::DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: 0,
        };
        let sc: IDXGISwapChain1 = factory
            .CreateSwapChainForHwnd(&dev, hwnd, &desc, None, None)
            .expect("CreateSwapChainForHwnd(d3d11)");
        let bb: ID3D11Texture2D = sc.GetBuffer(0).expect("GetBuffer");
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        dev.CreateRenderTargetView(&bb, None, Some(&mut rtv))
            .expect("CreateRenderTargetView");
        let rtv = rtv.expect("rtv");
        let mut msg = MSG::default();
        let start = std::time::Instant::now();
        let mut frame: u32 = 0;
        let mut last = std::time::Instant::now();
        let mut last_frames: u32 = 0;
        println!("[probe] D3D11 渲染循环开始（600s），等待覆盖层画上来");
        while start.elapsed().as_secs_f64() < 600.0 {
            while PeekMessageW(&mut msg, None, 0, 0, PEEK_MESSAGE_REMOVE_TYPE(1)).0 != 0 {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            let c0 = if frame % 2 == 0 { 0.05f32 } else { 0.15f32 };
            ctx.ClearRenderTargetView(&rtv, &[c0, 0.1, 0.25, 1.0]);
            let _ = sc.Present(1, DXGI_PRESENT(0));
            frame += 1;
            std::thread::sleep(std::time::Duration::from_millis(16));
            if last.elapsed().as_secs_f64() >= 1.0 {
                let dt = last.elapsed().as_secs_f64();
                println!(
                    "[probe] D3D11 t={:.0}s 帧率≈{:.0}/s 累计={}",
                    start.elapsed().as_secs_f64(),
                    (frame - last_frames) as f64 / dt,
                    frame
                );
                last = std::time::Instant::now();
                last_frames = frame;
            }
        }
        let _ = H::default();
    }
}