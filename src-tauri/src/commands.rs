use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tauri::{Emitter, Manager};
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_LWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsWindowVisible};

/// 配置文件路径：{app_data_dir}/config.json
fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("config.json"))
}

const DEFAULT_CONFIG: &str = r#"{
  "settings": {
    "compactMode": false,
    "airplaneMode": false
  },
  "games": [],
  "widgets": {
    "audio": true,
    "performance": true,
    "display": false,
    "quick": true
  }
}"#;

/// 配置写入串行化：多个窗口可能同时保存，read-modify-write 必须互斥，
/// 否则「读到旧副本 → 整份覆盖写」会互相抹掉对方的改动。
fn config_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn lock_config() -> MutexGuard<'static, ()> {
    config_lock().lock().unwrap_or_else(|e| e.into_inner())
}

/// 读取磁盘配置：缺失则写入默认；损坏（中断写入/手工编辑出错）则备份后回退默认，保证应用可用
fn read_config(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        std::fs::write(path, DEFAULT_CONFIG).map_err(|e| e.to_string())?;
        return serde_json::from_str(DEFAULT_CONFIG).map_err(|e| e.to_string());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    match serde_json::from_str(&raw) {
        Ok(v) => Ok(v),
        Err(e) => {
            let bak = path.with_extension("json.bak");
            let _ = std::fs::copy(path, &bak);
            std::fs::write(path, DEFAULT_CONFIG).map_err(|e2| e2.to_string())?;
            log::warn!(
                "config.json 解析失败（{e}），已重置为默认并备份到 {}",
                bak.display()
            );
            serde_json::from_str(DEFAULT_CONFIG).map_err(|e2| e2.to_string())
        }
    }
}

/// 原子写：先写 .tmp 再改名，避免其他窗口读到写了一半的损坏配置
fn write_config(path: &Path, cfg: &Value) -> Result<(), String> {
    let pretty = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, pretty).map_err(|e| e.to_string())?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.to_string())
        }
    }
}

/// 写入后的副作用 + 全窗口广播。任何配置写入都必须广播 config://updated：
/// 别的窗口如果不刷新，下次保存时就会用旧副本整份覆盖掉本次改动。
fn after_write(app: &tauri::AppHandle, cfg: &Value) {
    // 手柄控制开关（settings.gamepad，缺省开启）实时生效
    let on = cfg
        .get("settings")
        .and_then(|s| s.get("gamepad"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    crate::gamepad::set_enabled(on);
    let _ = app.emit("config://updated", cfg.clone());
}

#[tauri::command]
pub async fn load_config(app: tauri::AppHandle) -> Result<Value, String> {
    let path = config_path(&app)?;
    read_config(&path)
}

/// 供 perf 模块在后台驱动 FPS 悬浮窗：读取开关与角落位置。
/// settings.fpsOverlay（bool）/ settings.fpsOverlayPos（tl | tr | bl | br）
pub(crate) fn fps_overlay_prefs(app: &tauri::AppHandle) -> (bool, String) {
    let cfg = match config_path(app).ok().and_then(|p| read_config(&p).ok()) {
        Some(c) => c,
        None => return (false, "tl".into()),
    };
    let settings = cfg.get("settings");
    let enabled = settings
        .and_then(|s| s.get("fpsOverlay"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let pos = settings
        .and_then(|s| s.get("fpsOverlayPos"))
        .and_then(|v| v.as_str())
        .unwrap_or("tl")
        .to_string();
    (enabled, pos)
}

/// 供 perf 模块判定「窗口化游戏」：用户游戏库里所有条目的 exe 主文件名（小写、无扩展名）。
/// 帧率自采删除后，窗口没铺满屏幕时不再有「呈现速率」可用，只能按游戏库认人。
pub(crate) fn game_exe_names(app: &tauri::AppHandle) -> Vec<String> {
    let cfg = match config_path(app).ok().and_then(|p| read_config(&p).ok()) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    let arr = match cfg.get("games").and_then(|g| g.as_array()) {
        Some(a) => a,
        None => return out,
    };
    for g in arr {
        // 游戏库条目的字段名是 exePath（前端 src/settings/App.vue 一律读写 exePath）。
        // 这里原先只读 "exe"，导致返回值恒为空 → 窗口化游戏永远认不出来，覆盖层注入
        // 的「只对游戏库进程」过滤也会把所有游戏挡在门外。"exe" 作为兜底保留。
        let exe = g
            .get("exePath")
            .and_then(|v| v.as_str())
            .or_else(|| g.get("exe").and_then(|v| v.as_str()))
            .unwrap_or_default();
        if exe.is_empty() {
            continue;
        }
        let stem = std::path::Path::new(exe)
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !stem.is_empty() && !out.contains(&stem) {
            out.push(stem);
        }
    }
    out
}

/// 供 overlay_inject 使用：settings.dllInject（bool，默认关）——打开后会对检测到的
/// 前台游戏注入 egb_hook.dll，把悬浮条画面直接画进游戏交换链（独占全屏也能看见）。
/// 有被反作弊识别的风险，所以默认关闭、由用户在设置中心自行开启。
pub(crate) fn dll_inject_enabled(app: &tauri::AppHandle) -> bool {
    let cfg = match config_path(app).ok().and_then(|p| read_config(&p).ok()) {
        Some(c) => c,
        None => return false,
    };
    cfg.get("settings")
        .and_then(|s| s.get("dllInject"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 整份覆盖写（兼容入口，会广播）。新代码优先用 update_config 按 key 增量更新。
#[tauri::command]
pub async fn save_config(app: tauri::AppHandle, config: Value) -> Result<(), String> {
    let _guard = lock_config();
    let path = config_path(&app)?;
    write_config(&path, &config)?;
    after_write(&app, &config);
    Ok(())
}

/// 按 key 的增量更新（推荐入口）：
/// - settings / widgets：按键合并，值为 null 表示删除该键
/// - games 等其余顶层键：整体替换
/// 先在磁盘最新配置上合并再写回，写完广播 config://updated，返回合并后的完整配置。
#[tauri::command]
pub async fn update_config(app: tauri::AppHandle, patch: Value) -> Result<Value, String> {
    let _guard = lock_config();
    let path = config_path(&app)?;
    let mut cur = read_config(&path)?;
    merge_patch(&mut cur, patch);
    write_config(&path, &cur)?;
    after_write(&app, &cur);
    Ok(cur)
}

/// 把 patch 合并进当前配置：settings/widgets 按键合并，其余顶层键整体替换
fn merge_patch(cur: &mut Value, patch: Value) {
    let patch = match patch.as_object() {
        Some(o) => o.clone(),
        None => return,
    };
    if !cur.is_object() {
        *cur = json!({});
    }
    let cur = cur.as_object_mut().expect("已确保为对象");
    for (key, val) in patch {
        match key.as_str() {
            "settings" | "widgets" => {
                if !val.is_object() {
                    cur.insert(key, val);
                    continue;
                }
                let section = cur.entry(key).or_insert_with(|| json!({}));
                if !section.is_object() {
                    *section = json!({});
                }
                let section = section.as_object_mut().expect("已确保为对象");
                for (k, v) in val.as_object().expect("已确保为对象").clone() {
                    // null 表示删除该键（前端 diff 出来的删除项）
                    if v.is_null() {
                        section.remove(&k);
                    } else {
                        section.insert(k, v);
                    }
                }
            }
            _ => {
                cur.insert(key, val);
            }
        }
    }
}

/// 导出配置到指定文件：抹掉敏感项（API Key）后写盘，供用户安全分享。
#[tauri::command]
pub async fn export_config(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let _guard = lock_config();
    let cp = config_path(&app)?;
    let mut cfg = read_config(&cp)?;
    // 抹掉所有敏感键：攻略助手 API Key 是唯一落盘的密钥。
    // 注意：whisper 路径、游戏库、皮肤等都不算密钥，保留以便换机快速恢复。
    if let Some(s) = cfg.get_mut("settings").and_then(|s| s.as_object_mut()) {
        s.remove("guideKey");
    }
    let pretty = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, pretty).map_err(|e| e.to_string())?;
    log::info!("配置已导出（已抹除 API Key）：{path}");
    Ok(())
}

/// 从文件导入配置：校验是合法 JSON 对象后，按 update_config 的合并语义写回，
/// 并广播 config://updated 让所有窗口刷新。API Key 不会出现在导出文件里，
/// 因此导入不会覆盖本机已填的 Key（合并语义：文件里没有的键保持原样）。
#[tauri::command]
pub async fn import_config(app: tauri::AppHandle, path: String) -> Result<Value, String> {
    let _guard = lock_config();
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取失败：{e}"))?;
    let patch: Value = serde_json::from_str(&raw).map_err(|e| format!("不是合法的 JSON：{e}"))?;
    if !patch.is_object() {
        return Err("配置文件顶层必须是 JSON 对象".into());
    }
    let cp = config_path(&app)?;
    let mut cur = read_config(&cp)?;
    merge_patch(&mut cur, patch);
    write_config(&cp, &cur)?;
    after_write(&app, &cur);
    log::info!("配置已导入：{path}");
    Ok(cur)
}

/// 导入自定义皮肤背景图：复制到 {app_data_dir}/skins（内容寻址去重），返回新路径。
#[tauri::command]
pub async fn import_skin_image(app: tauri::AppHandle, src: String) -> Result<String, String> {
    let from = PathBuf::from(&src);
    if !from.is_file() {
        return Err(format!("文件不存在：{src}"));
    }
    let raw = from
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let ext = if ["png", "jpg", "jpeg", "webp", "bmp", "gif", "avif"]
        .iter()
        .any(|x| *x == raw)
    {
        raw
    } else {
        "png".into()
    };
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("skins");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let out = dir.join(format!("{:016x}.{ext}", fnv1a(&src)));
    if !out.exists() {
        std::fs::copy(&from, &out).map_err(|e| e.to_string())?;
    }
    Ok(out.to_string_lossy().into_owned())
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 从 exe 提取关联图标，保存为 PNG（缓存于 {app_data_dir}/icon-cache）。
/// 用 SHDefExtractIcon 优先取 256px 大图标（逐级降级 96/48），避免小图拉伸发糊。
#[tauri::command]
pub async fn extract_exe_icon(app: tauri::AppHandle, exe_path: String) -> Result<String, String> {
    let src = PathBuf::from(&exe_path);
    if !src.exists() {
        return Err(format!("文件不存在：{exe_path}"));
    }
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("icon-cache");
    std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
    let out = cache_dir.join(format!("{:016x}_256.png", fnv1a(&exe_path)));
    if out.exists() {
        return Ok(out.to_string_lossy().into_owned());
    }

    let esc = |s: &str| s.replace('\'', "''");
    // SHDefExtractIcon 可按指定尺寸提取（含 256px Jumbo 图标）；无大图时逐级降级
    let script = format!(
        "Add-Type -AssemblyName System.Drawing; \
         Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class EgbIcon {{ \
         [DllImport(\"shell32.dll\", CharSet=CharSet.Unicode)] public static extern int SHDefExtractIcon(string p, int i, uint f, out IntPtr l, out IntPtr s, uint sz); \
         [DllImport(\"user32.dll\")] public static extern bool DestroyIcon(IntPtr h); }}'; \
         foreach($z in @(256,96,48)){{ \
           $l=[IntPtr]::Zero; $s=[IntPtr]::Zero; \
           $r=[EgbIcon]::SHDefExtractIcon('{}',0,0,[ref]$l,[ref]$s,$z); \
           if($r -eq 0 -and $l -ne [IntPtr]::Zero){{ \
             [System.Drawing.Icon]::FromHandle($l).ToBitmap().Save('{}',[System.Drawing.Imaging.ImageFormat]::Png); \
             [EgbIcon]::DestroyIcon($l)|Out-Null; exit 0 \
           }} \
         }}; exit 1",
        esc(&exe_path),
        esc(&out.to_string_lossy())
    );
    let status = crate::spawn_no_window("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() || !out.exists() {
        return Err("图标提取失败".into());
    }
    Ok(out.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn launch_process(exe_path: String) -> Result<(), String> {
    if exe_path.starts_with("steam://") {
        Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(&exe_path)
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let p = PathBuf::from(&exe_path);
    if !p.exists() {
        return Err(format!("文件不存在：{exe_path}"));
    }
    let is_exe = p
        .extension()
        .map(|e| e.eq_ignore_ascii_case("exe"))
        .unwrap_or(false);
    if is_exe {
        // 游戏普遍要求工作目录为其所在目录；直启失败（如需提权 os error 740）回退 shell
        let spawned = Command::new(&p)
            .current_dir(p.parent().unwrap_or(&p))
            .spawn();
        if spawned.is_err() {
            crate::spawn_no_window("cmd")
                .args(["/C", "start", ""])
                .arg(&p)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
    } else {
        crate::spawn_no_window("cmd")
            .args(["/C", "start", ""])
            .arg(&p)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 扫描目录（两层深）中的 exe，过滤卸载/安装器等非游戏程序。
#[tauri::command]
pub async fn scan_directory(dir: String) -> Result<Vec<Value>, String> {
    let root = PathBuf::from(&dir);
    if !root.is_dir() {
        return Err(format!("目录不存在：{dir}"));
    }
    let skip_keywords = [
        "uninstall", "uninst", "setup", "crash", "report", "redist",
        "vcredist", "dxsetup", "dotnet", "launcher_crash", "bug", "eac",
        "easyanticheat", "helper", "update", "install",
    ];
    let mut found: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in std::fs::read_dir(&root).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let sub = std::fs::read_dir(&path).map_err(|e| e.to_string())?;
            for e2 in sub.flatten() {
                collect_exe(e2.path(), &skip_keywords, &mut found, &mut seen);
            }
        } else {
            collect_exe(path, &skip_keywords, &mut found, &mut seen);
        }
    }
    found.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    Ok(found)
}

fn collect_exe(
    path: PathBuf,
    skip_keywords: &[&str],
    found: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<PathBuf>,
) {
    let is_exe = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("exe"))
        .unwrap_or(false);
    if !is_exe || !seen.insert(path.clone()) {
        return;
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = name.to_lowercase();
    if skip_keywords.iter().any(|k| lower.contains(k)) {
        return;
    }
    found.push(json!({ "name": name, "path": path.to_string_lossy() }));
}

/// 已知电源计划 GUID；存在性/激活态由 powercfg 输出判断。
const KNOWN_PLANS: &[(&str, &str)] = &[
    ("a1841308-3541-4fab-bc81-f71556f20b4a", "节能"),
    ("381b4222-f694-41f0-9685-ff5bb260df2e", "平衡"),
    ("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c", "高性能"),
    ("e9a42b02-d5df-448d-aa00-03f14749eb61", "卓越性能"),
];

fn powercfg_output(args: &[&str]) -> Option<String> {
    let out = crate::spawn_no_window("powercfg").args(args).output().ok()?;
    // powercfg 中文系统输出为 GBK，按字节宽松转 UTF-8（GUID 为 ASCII，不受影响）
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.to_string())
}

#[tauri::command]
pub async fn list_power_plans() -> Result<Vec<Value>, String> {
    let listing = powercfg_output(&["/list"]).ok_or("无法读取电源计划")?;
    let active_raw = powercfg_output(&["/getactivescheme"]).unwrap_or_default();

    fn extract_guid(line: &str) -> Option<String> {
        let idx = line.to_uppercase().find("GUID")?;
        let rest = &line[idx + 4..];
        let filtered: String = rest
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect();
        let guid = filtered.trim_matches('-').to_lowercase();
        if guid.len() == 36 {
            Some(guid)
        } else {
            None
        }
    }

    let mut plans: Vec<Value> = Vec::new();
    for (guid, name) in KNOWN_PLANS {
        let exists = listing
            .lines()
            .any(|l| extract_guid(l).as_deref() == Some(*guid));
        let active = extract_guid(&active_raw).as_deref() == Some(*guid);
        plans.push(json!({ "guid": guid, "name": name, "exists": exists, "active": active }));
    }
    // 用户自定义计划（不在已知列表中）也一并列出
    for line in listing.lines() {
        if let Some(guid) = extract_guid(line) {
            if !KNOWN_PLANS.iter().any(|(g, _)| *g == guid) {
                let name = line
                    .split('(')
                    .last()
                    .map(|s| s.trim_end_matches(|c| c == ')' || c == '\u{d}' || c == '\n'))
                    .unwrap_or("自定义计划")
                    .trim()
                    .to_string();
                let name = if name.is_empty() { "自定义计划".to_string() } else { name };
                let active = extract_guid(&active_raw).as_deref() == Some(guid.as_str());
                plans.push(json!({ "guid": guid, "name": name, "exists": true, "active": active }));
            }
        }
    }
    Ok(plans)
}

#[tauri::command]
pub async fn set_power_plan(guid: String) -> Result<(), String> {
    // 简单校验：仅接受 GUID 形态的参数
    if guid.len() != 36 || !guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err("非法的电源计划 GUID".into());
    }
    let status = crate::spawn_no_window("powercfg")
        .args(["/setactive", &guid])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("切换电源计划失败".into());
    }
    Ok(())
}

/// 弹出 Windows 虚拟键盘。
/// 优先拉起**必定可见**的传统「屏幕键盘」osk.exe；osk 不可用时才回退
/// 现代「触摸键盘」TabTip（ITipInvocation::Toggle）。
#[tauri::command]
pub async fn show_virtual_keyboard() -> Result<(), String> {
    open_virtual_keyboard()
}

/// 同步实现：① 注入系统快捷键 Win+Ctrl+O → ② 直接拉起 osk.exe → ③ TabTip 兜底。
///
/// 思路与 Xbox Game Bar 一致：**不自己实现键盘，而是触发系统已有的能力**
/// （Game Bar 用 Win+Alt+K 呼出它的游戏键盘）。本项目 HDR 的 Win+Alt+B、
/// 截图的 Win+Shift+S 也是同一套 SendInput 注入。
/// 选 Win+Ctrl+O 而不是 Win+Alt+K：前者是微软官方无障碍快捷键、Win10/11 通用
/// 且不依赖 Game Bar 是否添加了「键盘」组件；后者在 Game Bar 被禁用或未添加
/// 组件时完全无响应。
pub fn open_virtual_keyboard() -> Result<(), String> {
    // Win+Ctrl+O 是**开关**：键盘已在显示时重复注入会把它关掉，
    // 在用户看来又成了"点了没反应"，故先判重。
    if osk_visible() {
        return Ok(());
    }
    if let Ok(()) = send_osk_hotkey() {
        return Ok(());
    }
    // 注入不可用（极少见）→ 直接拉起 osk.exe（必定可见）
    match spawn_osk() {
        Ok(()) => Ok(()),
        Err(e) => toggle_tabtip_sync().map_err(|e2| format!("{e}；触摸键盘同样失败：{e2}")),
    }
}

/// 发送系统官方快捷键 Win+Ctrl+O（打开屏幕键盘 OSK）。
/// 与手按快捷键完全等效，由系统自己完成全部唤起流程。
fn send_osk_hotkey() -> Result<(), String> {
    let vk_o = VIRTUAL_KEY(0x4F); // 'O'
    let press = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [
        press(VK_LWIN, false),
        press(VK_CONTROL, false),
        press(vk_o, false),
        press(vk_o, true),
        press(VK_CONTROL, true),
        press(VK_LWIN, true),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(format!("快捷键注入失败（{sent}/{}）", inputs.len()));
    }
    Ok(())
}

/// 屏幕键盘（osk.exe）是否已经显示在屏幕上
fn osk_visible() -> bool {
    use std::os::windows::ffi::OsStrExt;
    let cls: Vec<u16> = std::ffi::OsStr::new("OSKMainClass")
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        match FindWindowW(PCWSTR(cls.as_ptr()), PCWSTR::null()) {
            Ok(h) => !h.is_invalid() && IsWindowVisible(h).as_bool(),
            Err(_) => false,
        }
    }
}

/// 在独立 STA 线程切换 TabTip（触摸键盘），600ms 超时
fn toggle_tabtip_sync() -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        unsafe {
            // 触摸键盘 COM 服务要求调用方处于 STA 套间
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let ok = show_tabtip().is_ok();
            CoUninitialize();
            let _ = tx.send(ok);
        }
    });
    match rx.recv_timeout(Duration::from_millis(600)) {
        Ok(true) => Ok(()),
        Ok(false) => Err("TabTip 唤起失败".into()),
        Err(_) => Err("TabTip 唤起超时".into()),
    }
}

/// 经 COM（ITipInvocation::Toggle）唤起 TabTip 触摸键盘
fn show_tabtip() -> Result<(), String> {
    unsafe {
        let tip: ITipInvocation = CoCreateInstance(&CLSID_TABTIP, None, CLSCTX_LOCAL_SERVER)
            .map_err(|e| format!("TabTip 初始化失败：{e}"))?;
        tip.Toggle(HWND::default())
            .ok()
            .map_err(|e| format!("TabTip 唤起失败：{e}"))?;
    }
    Ok(())
}

/// 回退：直接启动传统屏幕键盘 osk.exe（必定可见）
fn spawn_osk() -> Result<(), String> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let osk = PathBuf::from(windir).join("System32\\osk.exe");
    if osk.exists() {
        Command::new(&osk)
            .spawn()
            .map_err(|e| format!("无法启动屏幕键盘：{e}"))?;
        Ok(())
    } else {
        Err("系统中未找到可用的屏幕键盘（osk.exe）".into())
    }
}

/// TabTip COM 组件（Windows 触摸键盘服务）
const CLSID_TABTIP: GUID = GUID::from_u128(0x4ce576fa_83dc_4f88_951c_9d0782b4e376);

/// 触摸键盘 COM 接口：Toggle(HWND) 切换显示/隐藏
/// 方法名须与 COM vtable 一致（C++ 约定 Toggle），故保留大写，non_snake_case 警告可忽略。
#[windows::core::interface("37c994e7-432b-4834-a2f7-dce1f13b9378")]
unsafe trait ITipInvocation: windows::core::IUnknown {
    fn Toggle(&self, hwnd: HWND) -> HRESULT;
}

/// 默认文件保存目录（图片库，取不到时退回文档库 / 用户目录）
#[tauri::command]
pub async fn default_save_dir() -> Result<String, String> {
    Ok(dirs::picture_dir()
        .or_else(dirs::document_dir)
        .or_else(|| dirs::home_dir())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into()))
}

/// 在资源管理器中打开目录（或文件所在位置）
#[tauri::command]
pub async fn open_in_explorer(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在：{path}"));
    }
    Command::new("explorer")
        .arg(&p)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 允许跳转的系统设置页（白名单，避免任意 URI）
const MS_SETTINGS_PAGES: &[&str] = &[
    "bluetooth",
    "network",
    "network-wifi",
    "network-airplane-mode",
];

/// 前端写日志到 Rust 日志文件：子窗口（game-intro / quick-drawer 等）没有可捞的
/// console，排查「点了没反应」时靠它留痕。level: info / warn / error。
#[tauri::command]
pub fn debug_log(msg: String, level: Option<String>) {
    match level.unwrap_or_else(|| "info".into()).as_str() {
        "warn" => log::warn!("[js] {msg}"),
        "error" => log::error!("[js] {msg}"),
        _ => log::info!("[js] {msg}"),
    }
}

/// 跳转 Windows 系统设置页（ms-settings:…）。
/// 用 ShellExecuteW 交给系统 URL 处理器：以管理员身份运行时也能正确拉起设置。
#[tauri::command]
pub async fn open_system_page(page: String) -> Result<(), String> {
    if !MS_SETTINGS_PAGES.contains(&page.as_str()) {
        return Err(format!("不支持的设置页：{page}"));
    }
    let uri = windows::core::HSTRING::from(format!("ms-settings:{page}"));
    let ret = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            &uri,
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // ShellExecute 约定：返回值 > 32 视为成功
    if (ret.0 as isize) <= 32 {
        return Err(format!("无法打开系统设置（ms-settings:{page}）"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patch_merges_settings_by_key_and_keeps_others() {
        let mut cur = json!({
            "settings": {"a": 1, "fpsOverlay": true},
            "widgets": {"audio": true},
            "games": [{"id": "g1"}]
        });
        merge_patch(&mut cur, json!({ "settings": { "a": 2 } }));
        assert_eq!(cur["settings"]["a"], 2);
        assert_eq!(cur["settings"]["fpsOverlay"], true); // 未提交的键原样保留
        assert_eq!(cur["widgets"]["audio"], true);
        assert_eq!(cur["games"][0]["id"], "g1");
    }

    #[test]
    fn patch_null_deletes_key() {
        let mut cur = json!({ "settings": { "skin": { "id": "x" }, "gamepad": true } });
        merge_patch(&mut cur, json!({ "settings": { "skin": null } }));
        assert!(cur["settings"].get("skin").is_none());
        assert_eq!(cur["settings"]["gamepad"], true);
    }

    #[test]
    fn patch_replaces_games_wholesale() {
        let mut cur = json!({ "games": [{"id": "old"}], "settings": { "a": 1 } });
        merge_patch(&mut cur, json!({ "games": [{"id": "new"}] }));
        assert_eq!(cur["games"], json!([{"id": "new"}]));
        assert_eq!(cur["settings"]["a"], 1);
    }

    #[test]
    fn patch_creates_missing_sections() {
        let mut cur = json!({});
        merge_patch(
            &mut cur,
            json!({ "settings": { "a": 1 }, "widgets": { "audio": false } }),
        );
        assert_eq!(cur["settings"]["a"], 1);
        assert_eq!(cur["widgets"]["audio"], false);
    }
}
