//! 开机自启：双机制按提权状态自动切换。
//!
//! - 提权运行（release 包 requireAdministrator）：注册表 Run 键拉不起提权应用
//!   （登录时会被 UAC 静默拦下），改用计划任务 `/RL HIGHEST` 随登录以管理员
//!   启动（管理员本人注册，无需再弹 UAC）。
//! - 非提权运行（dev）：HKCU\Software\Microsoft\Windows\CurrentVersion\Run，
//!   指向当前 exe（带引号，兼容含空格路径）。
//! 两种机制互为清理：开启一种时顺手删掉另一种的残留，避免双开。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    REG_SAM_FLAGS,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("EasyGamingBar");
const TASK_NAME: &str = "EasyGamingBar";

/// 当前进程是否持有提权令牌（release 包恒为 true）
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = Default::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut TOKEN_ELEVATION as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

fn schtasks(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    crate::spawn_no_window("schtasks")
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW，不闪黑框
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn task_query() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
}

fn task_create(exe: &std::path::Path) -> bool {
    // /RL HIGHEST：登录后直接以管理员启动，不再弹 UAC
    schtasks(&[
        "/Create",
        "/F",
        "/TN",
        TASK_NAME,
        "/TR",
        &format!("\"{}\"", exe.display()),
        "/SC",
        "ONLOGON",
        "/RL",
        "HIGHEST",
    ])
}

fn task_delete() {
    let _ = schtasks(&["/Delete", "/F", "/TN", TASK_NAME]);
}

/// 是否已配置开机自启（计划任务或注册表 Run 值任一存在）
#[tauri::command]
pub fn get_autostart() -> bool {
    if is_elevated() {
        task_query() || registry_value_exists()
    } else {
        registry_value_exists() || task_query()
    }
}

/// 设置/取消开机自启（按提权状态选机制，并清理另一种机制的残留）
#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let elevated = is_elevated();
    if enabled {
        if elevated {
            let exe = std::env::current_exe().map_err(|_| "获取程序路径失败")?;
            if !task_create(&exe) {
                return Err("创建计划任务失败".into());
            }
            registry_delete(); // 清理可能的历史残留，避免登录双开
            Ok(())
        } else {
            registry_set()?;
            task_delete(); // dev 环境下清掉遗留任务
            Ok(())
        }
    } else {
        task_delete();
        registry_delete();
        Ok(())
    }
}

fn registry_set() -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        let ret = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            REG_SAM_FLAGS(KEY_SET_VALUE.0),
            None,
            &mut hkey,
            None,
        );
        if ret != ERROR_SUCCESS {
            return Err("打开注册表 Run 键失败".into());
        }
        let exe = std::env::current_exe().map_err(|_| "获取程序路径失败")?;
        let mut data: Vec<u8> = format!("\"{}\"", exe.display())
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        data.extend_from_slice(&[0, 0]); // UTF-16 NUL 结尾
        let ret = RegSetValueExW(hkey, VALUE_NAME, None, REG_SZ, Some(&data));
        let _ = RegCloseKey(hkey);
        if ret == ERROR_SUCCESS {
            Ok(())
        } else {
            Err("写入注册表值失败".into())
        }
    }
}

fn registry_delete() {
    unsafe {
        let mut hkey = HKEY::default();
        let ret = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            REG_SAM_FLAGS(KEY_SET_VALUE.0),
            None,
            &mut hkey,
            None,
        );
        if ret != ERROR_SUCCESS {
            return;
        }
        // 值本就不存在（ERROR_FILE_NOT_FOUND）视为已删除
        let _ = RegDeleteValueW(hkey, VALUE_NAME);
        let _ = RegCloseKey(hkey);
    }
}

fn registry_value_exists() -> bool {
    let mut len = 0u32;
    let ret = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut len),
        )
    };
    ret == ERROR_SUCCESS
}
