// 测试三个已知 IPolicyConfig 接口 IID，找到本系统可用的那个
use windows::core::{interface, PCWSTR, IUnknown};
use windows::Win32::Media::Audio::ERole;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx};

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig10: ::windows_core::IUnknown {
    fn M1(&self, a: *const core::ffi::c_void, b: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M2(&self, a: *const core::ffi::c_void, b: bool, c: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M3(&self, a: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn M4(&self, a: *const core::ffi::c_void, b: *mut core::ffi::c_void, c: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M5(&self, a: *const core::ffi::c_void, b: bool, c: *mut i64, d: *mut i64) -> windows::core::Result<()>;
    fn M6(&self, a: *const core::ffi::c_void, b: *const i64) -> windows::core::Result<()>;
    fn M7(&self, a: *const core::ffi::c_void, b: *mut i32) -> windows::core::Result<()>;
    fn M8(&self, a: *const core::ffi::c_void, b: i32) -> windows::core::Result<()>;
    fn M9(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M10(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> windows::core::Result<()>;
    fn M12(&self, a: *const core::ffi::c_void, b: bool) -> windows::core::Result<()>;
}

#[interface("294935ce-f637-4e7c-a41b-ab255460b862")]
unsafe trait IPolicyConfigNew: ::windows_core::IUnknown {
    fn M1(&self, a: *const core::ffi::c_void, b: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M2(&self, a: *const core::ffi::c_void, b: bool, c: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M3(&self, a: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn M4(&self, a: *const core::ffi::c_void, b: *mut core::ffi::c_void, c: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M5(&self, a: *const core::ffi::c_void, b: bool, c: *mut i64, d: *mut i64) -> windows::core::Result<()>;
    fn M6(&self, a: *const core::ffi::c_void, b: *const i64) -> windows::core::Result<()>;
    fn M7(&self, a: *const core::ffi::c_void, b: *mut i32) -> windows::core::Result<()>;
    fn M8(&self, a: *const core::ffi::c_void, b: i32) -> windows::core::Result<()>;
    fn M9(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M10(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> windows::core::Result<()>;
    fn M12(&self, a: *const core::ffi::c_void, b: bool) -> windows::core::Result<()>;
}

#[interface("ca286fc3-91fd-42c3-8e9b-caafa66242e3")]
unsafe trait IPolicyConfigVista: ::windows_core::IUnknown {
    fn M1(&self, a: *const core::ffi::c_void, b: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M2(&self, a: *const core::ffi::c_void, b: bool, c: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M3(&self, a: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn M4(&self, a: *const core::ffi::c_void, b: *mut core::ffi::c_void, c: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M5(&self, a: *const core::ffi::c_void, b: bool, c: *mut i64, d: *mut i64) -> windows::core::Result<()>;
    fn M6(&self, a: *const core::ffi::c_void, b: *const i64) -> windows::core::Result<()>;
    fn M7(&self, a: *const core::ffi::c_void, b: *mut i32) -> windows::core::Result<()>;
    fn M8(&self, a: *const core::ffi::c_void, b: i32) -> windows::core::Result<()>;
    fn M9(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M10(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> windows::core::Result<()>;
    fn M12(&self, a: *const core::ffi::c_void, b: bool) -> windows::core::Result<()>;
}

// Win7 老版布局：SetDefaultEndpoint 在槽位 9
#[interface("568b9108-44bf-40b4-9006-86afe5b5a620")]
unsafe trait IPolicyConfig7: ::windows_core::IUnknown {
    fn M1(&self, a: *const core::ffi::c_void, b: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M2(&self, a: *const core::ffi::c_void, b: bool, c: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M3(&self, a: *const core::ffi::c_void, b: *mut core::ffi::c_void, c: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M4(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *mut core::ffi::c_void) -> windows::core::Result<()>;
    fn M5(&self, a: *const core::ffi::c_void, b: bool, c: *const core::ffi::c_void, d: *const core::ffi::c_void) -> windows::core::Result<()>;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> windows::core::Result<()>;
    fn M7(&self, a: *const core::ffi::c_void, b: bool) -> windows::core::Result<()>;
}

const CLSID: windows::core::GUID = windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn main() {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); };

    let outs = app_lib::audio::list_devices("output").unwrap();
    let target = outs.iter().find(|d| !d.is_default).expect("无第二设备");
    println!("目标设备: {}", target.name);
    let wide: Vec<u16> = target.id.encode_utf16().chain(std::iter::once(0)).collect();
    let id = PCWSTR(wide.as_ptr());

    // 记录原始默认输出，测试结束后还原（SetDefaultEndpoint 会真实改变系统状态）
    let original = app_lib::audio::default_device_id("output").ok();
    let restore = || {
        if let Some(orig) = &original {
            let _ = app_lib::audio::switch_default(orig);
            println!("已还原默认输出设备");
        }
    };

    unsafe {
        let p10: Result<IPolicyConfig10, _> = CoCreateInstance(&CLSID, None, CLSCTX_ALL);
        match p10 {
            Ok(p) => {
                println!("IID f8679f50 (Win10 版): CoCreateInstance 成功");
                let r = p.SetDefaultEndpoint(id, ERole(0))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(1)))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(2)));
                println!("  SetDefaultEndpoint: {:?}", r.err().map(|e| e.to_string()));
            }
            Err(e) => println!("IID f8679f50 (Win10 版): 激活失败: {}", e),
        }

        let pnew: Result<IPolicyConfigNew, _> = CoCreateInstance(&CLSID, None, CLSCTX_ALL);
        match pnew {
            Ok(p) => {
                println!("IID 294935ce (最新版): CoCreateInstance 成功");
                let r = p.SetDefaultEndpoint(id, ERole(0))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(1)))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(2)));
                println!("  SetDefaultEndpoint: {:?}", r.err().map(|e| e.to_string()));
            }
            Err(e) => println!("IID 294935ce (最新版): 激活失败: {}", e),
        }

        let pvis: Result<IPolicyConfigVista, _> = CoCreateInstance(&CLSID, None, CLSCTX_ALL);
        match pvis {
            Ok(p) => {
                println!("IID ca286fc3 (Vista 版): CoCreateInstance 成功");
                let r = p.SetDefaultEndpoint(id, ERole(0))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(1)))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(2)));
                println!("  SetDefaultEndpoint: {:?}", r.err().map(|e| e.to_string()));
            }
            Err(e) => println!("IID ca286fc3 (Vista 版): 激活失败: {}", e),
        }

        let p7: Result<IPolicyConfig7, _> = CoCreateInstance(&CLSID, None, CLSCTX_ALL);
        match p7 {
            Ok(p) => {
                println!("IID 568b9108 (Win7 版): CoCreateInstance 成功");
                let r = p.SetDefaultEndpoint(id, ERole(0))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(1)))
                    .and_then(|_| p.SetDefaultEndpoint(id, ERole(2)));
                println!("  SetDefaultEndpoint: {:?}", r.err().map(|e| e.to_string()));
            }
            Err(e) => println!("IID 568b9108 (Win7 版): 激活失败: {}", e),
        }
    }

    restore();
}
