//! 网络与无线电能力（Windows）：
//! - Wi-Fi / 蓝牙无线电开关：Windows.Devices.Radios（与系统快捷面板同一开关，
//!   Xbox Game Bar 快捷菜单亦使用此 API）
//! - 飞行模式：一次关/开所有无线电（Wi-Fi + 蓝牙）
//! - Wi-Fi 网络：netsh wlan 枚举/连接（免管理员权限，与系统行为一致）
//!   netsh 在中文系统输出 GBK，用 encoding_rs 解码
use serde::Serialize;
use windows::Devices::Radios::{Radio, RadioKind, RadioState};

/* ---------- 无线电（Wi-Fi / 蓝牙 / 飞行模式） ---------- */

fn radios_of_kind(kind: RadioKind) -> Result<Vec<Radio>, String> {
    let all = Radio::GetRadiosAsync()
        .map_err(|e| format!("无法访问无线电服务：{e}"))?
        .get()
        .map_err(|e| format!("枚举无线电失败：{e}"))?;
    let count = all.Size().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for i in 0..count {
        let r = all.GetAt(i).map_err(|e| e.to_string())?;
        if let Ok(k) = r.Kind() {
            if k == kind {
                out.push(r);
            }
        }
    }
    Ok(out)
}

fn radio_state_of(kind: RadioKind) -> Result<Option<RadioState>, String> {
    let list = radios_of_kind(kind)?;
    Ok(list.first().and_then(|r| r.State().ok()))
}

fn radio_enabled(kind: RadioKind) -> Result<bool, String> {
    Ok(radio_state_of(kind)? == Some(RadioState::On))
}

fn set_radio_enabled(kind: RadioKind, on: bool) -> Result<(), String> {
    let list = radios_of_kind(kind)?;
    if list.is_empty() {
        return Err(match kind {
            RadioKind::WiFi => "未找到 Wi-Fi 无线电设备",
            RadioKind::Bluetooth => "未找到蓝牙无线电设备",
            _ => "未找到无线电设备",
        }
        .into());
    }
    // 切换前请求无线电控制权限（桌面应用通常直接 Allowed）
    if let Ok(op) = Radio::RequestAccessAsync() {
        if let Ok(status) = op.get() {
            if matches!(
                status,
                windows::Devices::Radios::RadioAccessStatus::DeniedByUser
                    | windows::Devices::Radios::RadioAccessStatus::DeniedBySystem
            ) {
                return Err("系统拒绝了无线电控制权限".into());
            }
        }
    }
    let target = if on { RadioState::On } else { RadioState::Off };
    for r in &list {
        let status = r
            .SetStateAsync(target)
            .map_err(|e| format!("切换失败：{e}"))?
            .get()
            .map_err(|e| format!("切换失败：{e}"))?;
        if status == windows::Devices::Radios::RadioAccessStatus::DeniedByUser
            || status == windows::Devices::Radios::RadioAccessStatus::DeniedBySystem
        {
            return Err("系统拒绝了无线电切换请求".into());
        }
    }
    // 设备层翻转需要一两秒，等实际状态到位，UI 才能立即反映真实状态
    for _ in 0..20 {
        if radio_state_of(kind)? == Some(target) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err("切换命令已发出但状态未变化，请重试".into())
}

#[derive(Serialize)]
pub struct RadioStatus {
    pub wifi_on: bool,
    pub bt_on: bool,
    /// 飞行模式 = 所有已知无线电都处于关闭
    pub airplane: bool,
}

#[tauri::command]
pub async fn get_radio_status() -> Result<RadioStatus, String> {
    let wifi_on = radio_enabled(RadioKind::WiFi)?;
    let bt_on = radio_enabled(RadioKind::Bluetooth)?;
    Ok(RadioStatus {
        wifi_on,
        bt_on,
        airplane: !wifi_on && !bt_on,
    })
}

#[tauri::command]
pub async fn set_wifi_enabled(on: bool) -> Result<bool, String> {
    set_radio_enabled(RadioKind::WiFi, on)?;
    Ok(on)
}

#[tauri::command]
pub async fn set_bt_enabled(on: bool) -> Result<bool, String> {
    set_radio_enabled(RadioKind::Bluetooth, on)?;
    Ok(on)
}

/// 飞行模式：off→关全部无线电；on→重新打开 Wi-Fi 与蓝牙。
/// 某类无线电不存在时（如无蓝牙的台式机）跳过该项。
#[tauri::command]
pub async fn set_airplane_mode(on: bool) -> Result<bool, String> {
    let mut errs: Vec<String> = Vec::new();
    let mut acted = false;
    for kind in [RadioKind::WiFi, RadioKind::Bluetooth] {
        if radios_of_kind(kind).map(|l| l.is_empty()).unwrap_or(true) {
            continue;
        }
        acted = true;
        if let Err(e) = set_radio_enabled(kind, !on) {
            errs.push(e);
        }
    }
    if !acted {
        return Err("未找到可控制的无线电设备".into());
    }
    match errs.first() {
        Some(e) => Err(e.clone()),
        None => Ok(on),
    }
}

/* ---------- netsh 工具 ---------- */

/// 运行 netsh 并把（中文系统为 GBK 的）输出解码为字符串
fn netsh(args: &[&str]) -> Result<String, String> {
    let out = crate::spawn_no_window("netsh")
        .args(args)
        .output()
        .map_err(|e| format!("无法运行 netsh：{e}"))?;
    let (text, _, _) = encoding_rs::GBK.decode(&out.stdout);
    let text = text.into_owned();
    if !out.status.success() && text.trim().is_empty() {
        return Err(format!("netsh 执行失败（{:?}）", out.status.code()));
    }
    Ok(text)
}

/// 行按 "标签 : 值" 拆分，取值部分（中文/英文系统标签不同，故只按分隔符取右侧）
fn line_value(line: &str) -> Option<&str> {
    let idx = line.find(" : ")?;
    Some(line[idx + 3..].trim())
}

#[derive(Serialize, Clone)]
pub struct WifiNetwork {
    pub ssid: String,
    /// 0-100
    pub signal: u32,
    /// open | wpa2 | wpa3 | wep | unknown
    pub auth: String,
    pub connected: bool,
}

/// 当前连接（未连接时 connected=false）
#[derive(Serialize)]
pub struct WifiConnection {
    pub connected: bool,
    pub ssid: String,
    pub signal: u32,
}

fn parse_connection(text: &str) -> WifiConnection {
    let mut conn = WifiConnection {
        connected: false,
        ssid: String::new(),
        signal: 0,
    };
    for line in text.lines() {
        let t = line.trim();
        // "SSID" 独立标签（排除 BSSID 等含 SSID 字样的行）
        if (t.starts_with("SSID ") || t.starts_with("SSID:"))
            && !t.starts_with("BSSID")
            && t.contains(" : ")
        {
            let v = line_value(t).unwrap_or_default().to_string();
            if !v.is_empty() {
                conn.ssid = v;
            }
            continue;
        }
        // 信号行含 %（"信号 : 86%" / "Signal : 86%"）
        if let (Some(pct), Some(sep)) = (t.find('%'), t.find(" : ")) {
            if sep < pct {
                let digits: String = t[sep + 3..pct]
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(v) = digits.parse() {
                    conn.signal = v;
                }
            }
        }
    }
    conn.connected = !conn.ssid.is_empty();
    conn
}

#[derive(Serialize)]
pub struct WifiStatus {
    pub radio_on: bool,
    pub connection: WifiConnection,
}

#[tauri::command]
pub async fn get_wifi_status() -> Result<WifiStatus, String> {
    let radio_on = radio_enabled(RadioKind::WiFi).unwrap_or(false);
    let text = netsh(&["wlan", "show", "interfaces"]).unwrap_or_default();
    Ok(WifiStatus {
        radio_on,
        connection: parse_connection(&text),
    })
}

fn classify_auth(line: &str) -> &'static str {
    let t = line.to_lowercase();
    if t.contains("wpa3") {
        "wpa3"
    } else if t.contains("wpa2") || t.contains("wpa") || t.contains("802.1x") {
        "wpa2"
    } else if t.contains("wep") {
        "wep"
    } else if t.contains("open") || t.contains("开放") {
        "open"
    } else {
        "unknown"
    }
}

#[tauri::command]
pub async fn list_wifi_networks() -> Result<Vec<WifiNetwork>, String> {
    let text = netsh(&["wlan", "show", "networks"])?;
    let current = netsh(&["wlan", "show", "interfaces"])
        .map(|t| parse_connection(&t))
        .ok();

    let mut nets: Vec<WifiNetwork> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // "SSID 1 : MyWifi" / "SSID 1 :"（隐藏网络跳过）；排除 BSSID
        if !(t.starts_with("SSID ") && line.contains(" : ")) || t.starts_with("BSSID") {
            continue;
        }
        let ssid = line_value(line).unwrap_or_default().trim().to_string();
        if ssid.is_empty() {
            continue;
        }
        // 向下找该网络的验证方式与信号
        let mut auth = "unknown";
        let mut signal = 0u32;
        for la in text.lines().skip_while(|l| *l != line).skip(1) {
            let lt = la.trim();
            if lt.starts_with("SSID ") {
                break;
            }
            if lt.contains("身份验证") || lt.to_lowercase().contains("authentication") {
                auth = classify_auth(lt);
            } else if let Some(pct) = lt.find('%') {
                let digits: String = lt[..pct]
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(v) = digits.parse() {
                    signal = v;
                }
            }
        }
        let connected = current
            .as_ref()
            .map(|c| c.connected && c.ssid == ssid)
            .unwrap_or(false);
        if let Some(hit) = nets.iter_mut().find(|n| n.ssid == ssid) {
            hit.signal = hit.signal.max(signal);
        } else {
            nets.push(WifiNetwork {
                ssid,
                signal,
                auth: auth.into(),
                connected,
            });
        }
    }
    nets.sort_by(|a, b| b.signal.cmp(&a.signal).then(a.ssid.cmp(&b.ssid)));
    Ok(nets)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 生成 WLAN 配置文件 XML 并导入（免管理员：user=current）
fn add_profile(ssid: &str, auth: &str, password: Option<&str>) -> Result<(), String> {
    let (authentication, encryption, key_block) = match auth {
        "open" => ("open", "none", String::new()),
        "wpa3" => (
            "WPA3SAE",
            "SAE",
            format!(
                "<sharedKey><keyType>passPhrase</keyType><protected>false</protected>\
                 <keyMaterial>{}</keyMaterial></sharedKey>",
                xml_escape(password.unwrap_or_default())
            ),
        ),
        // 其余（wpa2/wep/unknown）按最常见 WPA2-PSK/AES 处理
        _ => (
            "WPA2PSK",
            "AES",
            format!(
                "<sharedKey><keyType>passPhrase</keyType><protected>false</protected>\
                 <keyMaterial>{}</keyMaterial></sharedKey>",
                xml_escape(password.unwrap_or_default())
            ),
        ),
    };
    let xml = format!(
        r#"<?xml version="1.0"?>
<WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1">
  <name>{name}</name>
  <SSIDConfig><SSID><name>{name}</name></SSID></SSIDConfig>
  <connectionType>ESS</connectionType>
  <connectionMode>auto</connectionMode>
  <MSM><security>
    <authEncryption><authentication>{auth}</authentication><encryption>{enc}</encryption><useOneX>false</useOneX></authEncryption>
    {key}
  </security></MSM>
</WLANProfile>"#,
        name = xml_escape(ssid),
        auth = authentication,
        enc = encryption,
        key = key_block
    );
    let path = std::env::temp_dir().join("easygamingbar-wifi.xml");
    std::fs::write(&path, xml).map_err(|e| format!("写入配置文件失败：{e}"))?;
    let fname = path.to_string_lossy().into_owned();
    let out = netsh(&[
        "wlan",
        "add",
        "profile",
        &format!("filename={fname}"),
        "user=current",
    ])?;
    let _ = std::fs::remove_file(&path);
    if !out.contains("成功") && !out.to_lowercase().contains("success") {
        // 个别语言系统提示语不同，无法可靠判断时交由 connect 结果兜底
        log::info!("add profile 输出：{}", out.trim());
    }
    Ok(())
}

/// 连接 Wi-Fi：已存配置直接连；加密网络无配置时需要密码（先导入配置）
#[tauri::command]
pub async fn connect_wifi(ssid: String, password: Option<String>) -> Result<(), String> {
    if ssid.is_empty() {
        return Err("SSID 不能为空".into());
    }
    let radio_on = radio_enabled(RadioKind::WiFi).unwrap_or(true);
    if !radio_on {
        return Err("Wi-Fi 已关闭，请先打开 Wi-Fi".into());
    }

    // 判断该网络加密类型；有配置文件的直接 connect
    let nets = list_wifi_networks().await.unwrap_or_default();
    let net = nets.iter().find(|n| n.ssid == ssid);
    let auth = net.map(|n| n.auth.as_str()).unwrap_or("unknown");

    let out = netsh(&["wlan", "connect", &format!("name={ssid}")]).unwrap_or_default();
    let launched = out.contains("成功") || out.to_lowercase().contains("success");
    if !launched {
        if auth == "open" {
            add_profile(&ssid, "open", None)?;
        } else {
            let pwd = password.as_deref().ok_or("该网络需要密码")?;
            if pwd.is_empty() {
                return Err("密码不能为空".into());
            }
            add_profile(&ssid, auth, Some(pwd))?;
        }
        netsh(&["wlan", "connect", &format!("name={ssid}")])?;
    }

    // 轮询确认连接结果（最多 ~12s）
    for _ in 0..24 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Ok(text) = netsh(&["wlan", "show", "interfaces"]) {
            let conn = parse_connection(&text);
            if conn.connected && conn.ssid == ssid {
                return Ok(());
            }
        }
    }
    Err("连接未成功，请检查密码或信号".into())
}

#[tauri::command]
pub async fn disconnect_wifi() -> Result<(), String> {
    netsh(&["wlan", "disconnect"]).map(|_| ())
}
