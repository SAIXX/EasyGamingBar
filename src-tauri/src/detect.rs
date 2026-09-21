//! 智能游戏检测：一键收集本机已安装游戏候选，用户勾选后导入。
//!
//! 傻瓜式导入的核心：用户不用再逐层点进游戏文件夹。候选来源（按优先级）：
//! 1. Steam 库 —— 复用 steam::scan_games（appmanifest 解析 + 封面缓存）；
//! 2. Epic —— %ProgramData%/Epic/EpicGamesLauncher/Data/Manifests/*.item 清单；
//! 3. GOG —— 注册表 HKLM\...\GOG.com\Games（gameName/path/exe）；
//! 4. 开始菜单快捷方式 —— 递归两层 Program\ 目录，COM 解析 .lnk 真身，
//!    过滤卸载器/启动器/系统目录后剩下的多半就是游戏。
//! 候选按 exe 路径去重（不分大小写），交给前端复选列表。

use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// 与 commands::scan_directory 同款的干扰项关键字（卸载器 / 运行库安装器等）
const SKIP_KEYWORDS: &[&str] = &[
    "uninstall", "uninst", "setup", "crash", "report", "redist", "vcredist",
    "dxsetup", "dotnet", "launcher_crash", "bug", "eac", "easyanticheat",
    "helper", "update", "install", "benchmark", "config", "settings",
    "readme", "licence", "license",
];

/// 启动器本体不是游戏，开始菜单里常见，直接排除
const LAUNCHER_EXES: &[&str] = &[
    "steam", "epicgameslauncher", "galaxyclient", "eadesktop", "origin",
    "ubisoftconnect", "battle.net", "xboxpc", "xboxapp", "discord",
    "unrealeditor", "sqlite3",
];

fn wanted(stem: &str) -> bool {
    let lower = stem.to_lowercase();
    if SKIP_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return false;
    }
    !LAUNCHER_EXES.iter().any(|k| lower == *k)
}

fn push_candidate(
    out: &mut Vec<Value>,
    seen: &mut HashSet<String>,
    name: &str,
    path: &str,
    app_id: Option<&str>,
    icon: Option<&str>,
    src: u8,
) {
    if name.is_empty() || path.is_empty() {
        return;
    }
    if !seen.insert(path.to_lowercase()) {
        return;
    }
    if !wanted(name) {
        return;
    }
    out.push(json!({
        "name": name,
        "path": path,
        "appId": app_id,
        "icon": icon,
        "src": src,
    }));
}

/// 一键扫描本机游戏候选（同步命令：tauri 自带线程池，阻塞无碍）
#[tauri::command]
pub fn smart_scan_games(app: tauri::AppHandle) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1) Steam：带 AppID（经 steam:// 启动）与封面
    if let Ok(cache_dir) = app.path().app_data_dir().map(|d| d.join("icon-cache")) {
        let _ = std::fs::create_dir_all(&cache_dir);
        if let Ok(games) = crate::steam::scan_games(&cache_dir) {
            for g in games {
                push_candidate(
                    &mut out,
                    &mut seen,
                    g["name"].as_str().unwrap_or(""),
                    g["exe"].as_str().unwrap_or(""),
                    Some(g["appid"].as_str().unwrap_or("")),
                    Some(g["icon"].as_str().unwrap_or("")),
                    0,
                );
            }
        }
    }

    // 2) Epic
    for (name, exe) in epic_games() {
        push_candidate(&mut out, &mut seen, &name, &exe, None, None, 1);
    }

    // 3) GOG
    for (name, exe) in gog_games() {
        push_candidate(&mut out, &mut seen, &name, &exe, None, None, 1);
    }

    // 4) 开始菜单快捷方式
    for (name, exe) in start_menu_games() {
        push_candidate(&mut out, &mut seen, &name, &exe, None, None, 2);
    }

    // 排序：来源优先（Steam/Epic/GOG 带正牌元数据的排前），同源内按名字 —— 
    // 开始菜单 .lnk 里的 7-Zip 等工具不该淹没真正的游戏
    let rank = |v: &Value| v.get("src").and_then(|s| s.as_u64()).unwrap_or(2) as u8;
    out.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
            })
    });
    Ok(out)
}

/* ---------------- Epic：清单文件 ---------------- */

fn epic_games() -> Vec<(String, String)> {
    let Some(program_data) = std::env::var("ProgramData").ok() else {
        return Vec::new();
    };
    let dir = PathBuf::from(program_data)
        .join("Epic")
        .join("EpicGamesLauncher")
        .join("Data")
        .join("Manifests");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension()
            .map(|x| !x.eq_ignore_ascii_case("item"))
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v["bIsInstalled"] == false {
            continue;
        }
        let name = v["DisplayName"].as_str().unwrap_or("").trim().to_string();
        let install = v["InstallLocation"].as_str().unwrap_or("");
        let exe_rel = v["LaunchExecutable"]
            .as_str()
            .unwrap_or("")
            .replace('/', "\\");
        if name.is_empty() || install.is_empty() || exe_rel.is_empty() {
            continue;
        }
        let exe = Path::new(install).join(exe_rel);
        if exe.is_file() {
            out.push((name, exe.to_string_lossy().into_owned()));
        }
    }
    out
}

/* ---------------- GOG：注册表 ---------------- */

fn gog_games() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for sub in [
        "SOFTWARE\\WOW6432Node\\GOG.com\\Games",
        "SOFTWARE\\GOG.com\\Games",
    ] {
        let Some(h_root) = reg_open(sub) else {
            continue;
        };
        for id in reg_subkeys(h_root) {
            let Some(h) = reg_open(&format!("{sub}\\{id}")) else {
                continue;
            };
            let name = reg_sz(&h, "gameName").unwrap_or_default();
            let dir = reg_sz(&h, "path").unwrap_or_default();
            let exe = reg_sz(&h, "exe").unwrap_or_default();
            unsafe {
                let _ = windows::Win32::System::Registry::RegCloseKey(h);
            }
            if name.is_empty() || dir.is_empty() || exe.is_empty() {
                continue;
            }
            let p = Path::new(&dir).join(exe.replace('/', "\\"));
            if p.is_file() {
                out.push((name, p.to_string_lossy().into_owned()));
            }
        }
        unsafe {
            let _ = windows::Win32::System::Registry::RegCloseKey(h_root);
        }
    }
    out
}

/* ---------------- 开始菜单：快捷方式解析 ---------------- */

fn start_menu_games() -> Vec<(String, String)> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(u) = std::env::var("USERPROFILE") {
        roots.push(PathBuf::from(u)
            .join("AppData")
            .join("Roaming")
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs"));
    }
    if let Ok(p) = std::env::var("ProgramData") {
        roots.push(PathBuf::from(p)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs"));
    }

    // 线程池线程可能已初始化过 COM（模式不同也算可用），错误一律忽略
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let mut lnks: Vec<PathBuf> = Vec::new();
    for root in &roots {
        collect_lnks(root, 0, &mut lnks);
    }
    lnks.into_iter().filter_map(|p| resolve_lnk(&p)).collect()
}

fn collect_lnks(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_lnks(&p, depth + 1, out);
        } else if p
            .extension()
            .map(|x| x.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            out.push(p);
        }
    }
}

/// COM 解析 .lnk 真身：目标必须是存在的 exe，且不在系统目录、非启动器
fn resolve_lnk(lnk: &Path) -> Option<(String, String)> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::System::Com::IPersistFile;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_UNCPRIORITY};

    let link_w: Vec<u16> = lnk
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: IPersistFile = sl.cast().ok()?;
        pf.Load(PCWSTR(link_w.as_ptr()), STGM_READ).ok()?;

        let mut buf = [0u16; 1024];
        let mut fd = WIN32_FIND_DATAW::default();
        sl.GetPath(&mut buf, &mut fd, SLGP_UNCPRIORITY.0 as u32).ok()?;

        let target = String::from_utf16_lossy(&buf)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        if target.is_empty() {
            return None;
        }
        let p = PathBuf::from(&target);
        if !p.is_file() {
            return None;
        }
        if p.extension()
            .map(|x| !x.eq_ignore_ascii_case("exe"))
            .unwrap_or(true)
        {
            return None;
        }
        if target.to_lowercase().contains("\\windows\\") {
            return None;
        }
        let name = p.file_stem()?.to_string_lossy().into_owned();
        if !wanted(&name) {
            return None;
        }
        Some((name, target))
    }
}

/* ---------------- 注册表小工具 ---------------- */

fn reg_open(sub: &str) -> Option<windows::Win32::System::Registry::HKEY> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};
    let w: Vec<u16> = sub.encode_utf16().chain([0]).collect();
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(w.as_ptr()), None, KEY_READ, &mut h) };
    (r.0 == 0).then_some(h)
}

fn reg_sz(h: &windows::Win32::System::Registry::HKEY, name: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::RegGetValueW;
    use windows::Win32::System::Registry::RRF_RT_REG_SZ;
    let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            *h,
            PCWSTR::null(),
            PCWSTR(w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.0 != 0 {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    Some(
        String::from_utf16_lossy(&buf[..n])
            .trim_end_matches('\0')
            .to_string(),
    )
}

fn reg_subkeys(h: windows::Win32::System::Registry::HKEY) -> Vec<String> {
    use windows::Win32::System::Registry::RegEnumKeyExW;
    let mut out = Vec::new();
    for idx in 0..u32::MAX {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                h,
                idx,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if r.0 != 0 || len == 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}
