//! Steam 库扫描：读注册表定位 Steam，解析 libraryfolders.vdf 与 appmanifest_*.acf，
//! 列出已安装游戏的主程序 exe 与本地封面图标（appcache/librarycache）。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::Manager;

/// 注册表 HKCU\Software\Valve\Steam\SteamPath → Steam 安装目录
fn steam_root() -> Option<PathBuf> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let sub = w!("Software\\Valve\\Steam");
    let name = w!("SteamPath");
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let ret = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if ret != ERROR_SUCCESS || len < 2 {
        return None;
    }
    let chars = (len as usize / 2).saturating_sub(1);
    let s = String::from_utf16_lossy(&buf[..chars]);
    let p = PathBuf::from(s.trim().trim_matches('"'));
    p.is_dir().then_some(p)
}

fn default_steam_root() -> Option<PathBuf> {
    let p = PathBuf::from(r"C:\Program Files (x86)\Steam");
    p.is_dir().then_some(p)
}

/// 从 VDF 文本提取 key 对应的值（"key" 后第一个带引号的字符串）
fn vdf_value(content: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let mut rest = content;
    while let Some(i) = rest.find(&pat) {
        let after = &rest[i + pat.len()..];
        let hit = after
            .find('"')
            // 键名与值之间应只有空白，避免匹配到含相同子串的其它键
            .filter(|&q1| after[..q1].chars().all(|c| c.is_whitespace()))
            .and_then(|q1| {
                after[q1 + 1..]
                    .find('"')
                    .map(|q2| after[q1 + 1..q1 + 1 + q2].to_string())
            });
        if hit.is_some() {
            return hit;
        }
        rest = after;
    }
    None
}

/// 库路径归一化（小写 + 统一斜杠），用于注册表路径与 vdf 路径去重
fn norm_lib(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// 全部 Steam 库目录（安装根 + libraryfolders.vdf 中声明的其它盘库）
fn library_paths(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    let vdf = root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(text) = std::fs::read_to_string(&vdf) {
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with("\"path\"") {
                if let Some(v) = vdf_value(t, "path") {
                    let p = PathBuf::from(v.replace("\\\\", "\\"));
                    if p.is_dir() && !libs.iter().any(|l| norm_lib(l) == norm_lib(&p)) {
                        libs.push(p);
                    }
                }
            }
        }
    }
    libs
}

/// 挑选主程序的排除关键词（卸载器 / 运行库安装器 / 崩溃收集器等）
const EXE_SKIP: &[&str] = &[
    "uninstall", "uninst", "setup", "crash", "report", "redist", "vcredist",
    "dxsetup", "dxwebsetup", "dotnet", "bug", "eac", "easyanticheat", "helper",
    "update", "install", "prelauncher", "touchup", "unity", "mono", "oalinst",
    "xnafx", "x3daudio", "steamerror", "vc_redist", "crashhandler", "server",
];

/// 搜索时整层跳过的目录（安装器 / 冗余资源），注意不放行游戏真身的 bin 目录
const DIR_SKIP: &[&str] = &[
    "__installer", "_commonredist", "commonredist", "redist", "directx",
    "dotnet", "support", "extras", "uninstall",
];

/// 归一化比较键：仅保留字母数字并小写（"Cyberpunk 2077" → "cyberpunk2077"）
fn norm_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn exe_score(p: &Path, hints: &[String]) -> Option<(i32, u64)> {
    let ext = p.extension()?.to_string_lossy();
    if !ext.eq_ignore_ascii_case("exe") || !p.is_file() {
        return None;
    }
    let stem = p.file_stem()?.to_string_lossy().to_lowercase();
    let low = stem.as_str();
    if EXE_SKIP.iter().any(|k| low.contains(k)) {
        return None;
    }
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let key = norm_key(&stem);
    let mut score = 10; // 普通候选也胜过没有
    for h in hints {
        let hk = norm_key(h);
        if hk.is_empty() {
            continue;
        }
        if key == hk {
            score = 100;
            break;
        }
        // 归一化后双向包含（len≥4 防误命中），如 bin/x64/Cyberpunk2077.exe
        if hk.len() >= 4 && (key.contains(&hk) || hk.contains(&key)) {
            score = score.max(60);
        }
    }
    // 深度惩罚在扫描处统一施加（同分优先浅层）
    Some((score, size))
}

/// 扫描单个目录（至多三层），把最优 exe 候选记入 best；
/// 评分高者胜，同分取体积大者，并按绝对深度惩罚使浅层优先。
fn scan_dir_for_exe(
    d: &Path,
    hints: &[String],
    depth_left: u8,
    abs_depth: u8,
    best: &mut Option<(i32, u64, PathBuf)>,
) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if depth_left > 0 && !DIR_SKIP.iter().any(|k| name.contains(k)) {
                subdirs.push(p);
            }
            continue;
        }
        if let Some((s, sz)) = exe_score(&p, hints) {
            let s = (s - abs_depth as i32 * 15).max(1);
            let better = best
                .as_ref()
                .map_or(true, |(bs, bsz, _)| s > *bs || (s == *bs && sz > *bsz));
            if better {
                *best = Some((s, sz, p));
            }
        }
    }
    if depth_left > 0 {
        for sd in subdirs {
            scan_dir_for_exe(&sd, hints, depth_left - 1, abs_depth + 1, best);
        }
    }
}

/// 在游戏目录挑主程序：命中 installdir / 游戏名（归一化）的加分，同级取体积大者
fn pick_exe(dir: &Path, hints: &[String]) -> Option<PathBuf> {
    let mut best: Option<(i32, u64, PathBuf)> = None;
    scan_dir_for_exe(dir, hints, 3, 0, &mut best);
    best.map(|(_, _, p)| p)
}

/// 封面文件优先级：0=竖版封面(capsule / 600x900，方形裁切画质最佳)
/// 1=客户端小图标(多 32px) 2=横版 header 3=logo(多为带透明标题图) 4=hero 大图兜底
fn icon_prio(name: &str) -> i32 {
    let n = name.to_lowercase();
    if n.contains("capsule") || n.contains("600x900") {
        0
    } else if !n.starts_with("library") && !n.contains("hero") && !n.starts_with("logo") {
        1 // 哈希名小图标（{appid}/ 根的 {hash}.jpg 或旧版 {appid}_icon.jpg）
    } else if n.contains("header") {
        2
    } else if n.starts_with("logo") {
        3
    } else {
        4
    }
}

/// Steam 本地封面：新版 librarycache/{appid}/（含一层哈希子目录），
/// 兼容旧版平铺 {appid}_icon.jpg。复制到 icon-cache 稳定保存；
/// 找不到返回 None（前端回退提取 exe 图标）。
fn steam_icon(steam_root: &Path, appid: &str, cache_dir: &Path) -> Option<PathBuf> {
    let bases = [
        steam_root.join("appcache").join("librarycache"),
        steam_root.join("steamapps").join("librarycache"),
    ];
    let mut pick: Option<(i32, PathBuf)> = None;
    'outer: for base in bases.iter() {
        // 搜集 {appid}/ 本身与其一层子目录（新版资产在哈希子目录内）
        let sub = base.join(appid);
        let mut dirs = vec![sub.clone()];
        if let Ok(rd) = std::fs::read_dir(&sub) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                }
            }
        }
        // 旧版平铺 {appid}_icon.jpg
        for ext in ["jpg", "png"] {
            let f = base.join(format!("{appid}_icon.{ext}"));
            if f.is_file() {
                let prio = icon_prio(&format!("{appid}_icon.{ext}"));
                if pick.as_ref().map_or(true, |(bp, _)| prio < *bp) {
                    pick = Some((prio, f));
                }
            }
        }
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let Some(name) = p.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                    continue;
                };
                let low = name.to_lowercase();
                if !(low.ends_with(".jpg") || low.ends_with(".png")) {
                    continue;
                }
                let prio = icon_prio(&name);
                if prio == 0 {
                    pick = Some((0, p));
                    break 'outer;
                }
                if pick.as_ref().map_or(true, |(bp, _)| prio < *bp) {
                    pick = Some((prio, p));
                }
            }
        }
    }
    let (_, src) = pick?;
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "jpg".into());
    let out = cache_dir.join(format!("steam_{appid}_icon.{ext}"));
    if !out.exists() {
        std::fs::copy(&src, &out).ok()?;
    }
    Some(out)
}

/// Steam 游戏简介（商店 appdetails 公开接口，无需鉴权），磁盘缓存于
/// app_data/steam-meta.json（含空结果，避免反复请求）；简中优先、英文回退。
#[tauri::command]
pub async fn fetch_steam_description(
    app: tauri::AppHandle,
    appid: String,
) -> Result<String, String> {
    if appid.is_empty() || !appid.chars().all(|c| c.is_ascii_digit()) {
        return Err("无效的 AppID".into());
    }
    let cache_path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("steam-meta.json");
    if let Ok(text) = std::fs::read_to_string(&cache_path) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(s) = v[appid.as_str()]["desc"].as_str() {
                return Ok(s.to_string());
            }
        }
    }
    let id = appid.clone();
    let desc = tauri::async_runtime::spawn_blocking(move || {
        let fetch = |url: String| -> Option<String> {
            let body = ureq::get(&url)
                .timeout(std::time::Duration::from_secs(8))
                .call()
                .ok()?
                .into_string()
                .ok()?;
            let v: Value = serde_json::from_str(&body).ok()?;
            v[id.as_str()]["data"]["short_description"]
                .as_str()
                .map(str::to_string)
        };
        fetch(format!(
            "https://store.steampowered.com/api/appdetails?appids={id}&l=schinese"
        ))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            fetch(format!(
                "https://store.steampowered.com/api/appdetails?appids={id}"
            ))
        })
        .unwrap_or_default()
    })
    .await
    .map_err(|e| e.to_string())?;
    // 写缓存
    let mut cache = std::fs::read_to_string(&cache_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    if let Value::Object(ref mut m) = cache {
        m.insert(appid.clone(), json!({ "desc": desc }));
    }
    if let Ok(text) = serde_json::to_string(&cache) {
        let _ = std::fs::write(&cache_path, text);
    }
    Ok(desc)
}

/// 从 localconfig.vdf 文本中提取指定 appid 条目（Playtime 分钟数 / LastPlayed）。
/// 结构："appid" 换行 { 换行 "Playtime" "n" ... }——按行 + 深度扫描即可，无需完整 VDF 解析器
fn extract_app_entry(
    text: &str,
    appid: &str,
) -> Option<(u64, Option<i64>)> {
    let lines: Vec<&str> = text.lines().collect();
    let needle = format!("\"{appid}\"");
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].trim() == needle {
            let mut k = i + 1;
            while k < lines.len() && lines[k].trim() != "{" {
                k += 1;
            }
            if k >= lines.len() {
                return None;
            }
            let mut depth: i32 = 1;
            let mut minutes: u64 = 0;
            let mut last: Option<i64> = None;
            k += 1;
            while k < lines.len() && depth > 0 {
                let l = lines[k].trim();
                depth += l.matches('{').count() as i32;
                depth -= l.matches('}').count() as i32;
                if depth == 1 {
                    if let Some(v) = vdf_value(l, "Playtime") {
                        minutes = v.parse().unwrap_or(minutes);
                    } else if last.is_none() {
                        if let Some(v) = vdf_value(l, "LastPlayed") {
                            last = v.parse().ok();
                        }
                    }
                }
                k += 1;
            }
            return Some((minutes, last));
        }
        i += 1;
    }
    None
}

/// Steam 游戏时长：读 userdata/<账号>/config/localconfig.vdf 的 Playtime（分钟）。
/// 多账号取并集累加（家庭共享场景）；非 Steam 游戏无此数据
#[derive(serde::Serialize)]
pub struct SteamPlaytime {
    pub appid: String,
    /// 累计游玩分钟数
    pub minutes: u64,
    /// 最近一次游玩（Unix 秒）
    pub last_played: Option<i64>,
}

#[tauri::command]
pub async fn steam_playtime(
    app: tauri::AppHandle,
    app_ids: Vec<String>,
) -> Result<Vec<SteamPlaytime>, String> {
    let _ = app;
    if app_ids.is_empty() {
        return Ok(Vec::new());
    }
    let root = steam_root().ok_or("未找到 Steam 安装目录")?;
    let userdata = root.join("userdata");
    let mut map: std::collections::HashMap<String, SteamPlaytime> = std::collections::HashMap::new();
    let accounts = match std::fs::read_dir(&userdata) {
        Ok(rd) => rd,
        Err(e) => return Err(format!("读取 userdata 失败：{e}")),
    };
    for entry in accounts.flatten() {
        let cfg = entry.path().join("config").join("localconfig.vdf");
        let Ok(text) = std::fs::read_to_string(&cfg) else {
            continue;
        };
        for id in &app_ids {
            if let Some((minutes, last)) = extract_app_entry(&text, id) {
                let e = map.entry(id.clone()).or_insert_with(|| SteamPlaytime {
                    appid: id.clone(),
                    minutes: 0,
                    last_played: None,
                });
                e.minutes += minutes;
                if e.last_played.is_none() {
                    e.last_played = last;
                }
            }
        }
    }
    Ok(map.into_values().collect())
}

/// 扫描 Steam 已安装游戏：[{appid, name, exe, icon}]（icon 为空串时前端回退提取）
#[tauri::command]
pub async fn scan_steam_games(app: tauri::AppHandle) -> Result<Vec<Value>, String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("icon-cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    scan_games(&cache_dir)
}

/// 扫描核心（独立于 AppHandle，便于示例自测）：只读 Steam 目录，
/// 仅把命中的封面复制进 cache_dir。
pub fn scan_games(cache_dir: &Path) -> Result<Vec<Value>, String> {
    let root = steam_root()
        .or_else(default_steam_root)
        .ok_or("未检测到 Steam 安装目录")?;

    let mut games: Vec<Value> = Vec::new();
    for lib in library_paths(&root) {
        let apps_dir = lib.join("steamapps");
        let rd = match std::fs::read_dir(&apps_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut manifests: Vec<(String, PathBuf)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("appmanifest_") && n.to_lowercase().ends_with(".acf")
                    })
                    .unwrap_or(false)
            })
            .map(|p| {
                let id = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().trim_start_matches("appmanifest_").to_string())
                    .unwrap_or_default();
                (id, p)
            })
            .collect();
        manifests.sort_by(|a, b| a.0.cmp(&b.0));

        for (appid, mf) in manifests {
            // 注册表根目录与 vdf 声明的库可能是同一目录（大小写/斜杠差异），按 appid 去重
            if games
                .iter()
                .any(|g| g["appid"].as_str() == Some(appid.as_str()))
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&mf) else {
                continue;
            };
            // StateFlags 位 4 = 已完全安装；缺失时以 common 目录存在为准
            let flags: u32 = vdf_value(&text, "StateFlags")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if flags != 0 && flags & 4 == 0 {
                continue;
            }
            let name = vdf_value(&text, "name").unwrap_or_else(|| appid.clone());
            let installdir = vdf_value(&text, "installdir").unwrap_or_default();
            let dir = apps_dir.join("common").join(&installdir);
            if !dir.is_dir() {
                continue; // 卸载残留清单
            }
            let hints = vec![installdir.clone(), name.clone()];
            let exe = pick_exe(&dir, &hints)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let icon = steam_icon(&root, &appid, cache_dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            games.push(json!({ "appid": appid, "name": name, "exe": exe, "icon": icon }));
        }
    }
    games.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    Ok(games)
}
