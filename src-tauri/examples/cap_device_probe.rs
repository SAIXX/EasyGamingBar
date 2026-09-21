//! 诊断：打印当前系统「默认采集设备」在 eConsole / eCommunications 两个角色下
//! 分别是谁，以及全部激活的输入设备列表。用于确认语音录音取错设备的问题。
#![allow(non_snake_case)]
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    IMMDeviceEnumerator, EDataFlow, ERole, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED, STGM,
};

fn name_of(dev: &windows::Win32::Media::Audio::IMMDevice) -> String {
    let store = match unsafe { dev.OpenPropertyStore(STGM(0)) } {
        Ok(s) => s,
        Err(_) => return "?".into(),
    };
    let pv = match unsafe { store.GetValue(&PKEY_Device_FriendlyName) } {
        Ok(v) => v,
        Err(_) => return "?".into(),
    };
    unsafe {
        if let Ok(pw) = PropVariantToStringAlloc(&pv) {
            let n = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as _));
            return n;
        }
    }
    "?".into()
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        for (label, role) in [
            ("eConsole(默认设备)", ERole(0)),
            ("eMultimedia", ERole(2)),
            ("eCommunications(默认通信设备)", ERole(1)),
        ] {
            match en.GetDefaultAudioEndpoint(EDataFlow(1), role) {
                Ok(d) => println!("[capture] {label} → {}", name_of(&d)),
                Err(e) => println!("[capture] {label} → 无（{e}）"),
            }
        }
        let coll = en
            .EnumAudioEndpoints(EDataFlow(1), DEVICE_STATE_ACTIVE)
            .unwrap();
        let n = coll.GetCount().unwrap();
        println!("—— 全部激活输入设备（{n}）——");
        for i in 0..n {
            let d = coll.Item(i).unwrap();
            println!("  · {}", name_of(&d));
        }
    }
}
