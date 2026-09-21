import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";

export interface GameItem {
  id: string;
  name: string;
  exePath: string;
  iconPath?: string;
  /** Steam 游戏 AppID；存在时经 steam://rungameid 启动 */
  steamAppId?: string;
  /** 本地统计的累计游玩秒数 */
  playSeconds?: number;
  /** 最近一次启动的 Unix 时间（秒） */
  lastPlayed?: number;
  /** 经悬浮条启动的次数 */
  launches?: number;
}

export interface AppConfig {
  settings: Record<string, unknown>;
  games: GameItem[];
  widgets: Record<string, boolean>;
}

export interface ScanResult {
  name: string;
  path: string;
  /** 智能扫描附带：Steam AppID（存在则经 steam:// 协议启动） */
  appId?: string;
  /** 智能扫描附带：Steam 封面缓存路径 */
  icon?: string;
}

/** Steam 库扫描结果（icon 为空串时由前端回退提取 exe 图标） */
export interface SteamGame {
  appid: string;
  name: string;
  exe: string;
  icon: string;
}

export const scanSteamGames = () => invoke<SteamGame[]>("scan_steam_games");

/** Steam 游戏时长（localconfig.vdf 的 Playtime 分钟数） */
export interface SteamPlaytimeInfo {
  appid: string;
  minutes: number;
  last_played: number | null;
}
export const steamPlaytime = (appIds: string[]) =>
  invoke<SteamPlaytimeInfo[]>("steam_playtime", { appIds });
/** Steam 商店短简介（Rust 侧磁盘缓存；离线/无简介返回空串） */
export const fetchSteamDescription = (appid: string) =>
  invoke<string>("fetch_steam_description", { appid });
/** 标记经悬浮条启动的游戏：Rust 后台跟踪进程存活时长，增量经 game://play-progress 广播 */
export const trackGameSession = (exePath: string) =>
  invoke("track_game_session", { exePath });
/** 导入自定义皮肤图片到 app_data_dir/skins，返回新路径 */
export const importSkinImage = (src: string) =>
  invoke<string>("import_skin_image", { src });

export interface PowerPlan {
  guid: string;
  name: string;
  exists: boolean;
  active: boolean;
}

export const loadConfig = () => invoke<AppConfig>("load_config");
/**
 * 覆盖模式下取焦点：悬浮条默认「不激活」地浮在游戏画面上，键盘仍归游戏；
 * 用户鼠标真正点进来时才用这个命令把焦点拿过来（键鼠与手柄导航都需要焦点）。
 */
export const focusOverlay = (label?: string) =>
  invoke("focus_overlay", { label });

/**
 * 隐藏自身前「把前台还给游戏」：窗口自己 hide 时若它正持有前台，Windows 会顺手
 * 把前台交给 Z 序里的下一个窗口（表现为「关掉面板后跳到桌面 / 任务视图」）。
 * 只有前台确实停在本应用窗口上时才动作，玩家中途 Alt+Tab 走了不会被拽回来。
 */
export const restoreFocus = () => invoke<boolean>("restore_focus");

/**
 * 覆盖窗口通用的「点击取焦点」：悬浮条 / 直播工具条 / 组件面板都是
 * NOACTIVATE 浮窗（点击不激活，游戏里键盘默认归游戏），必须点进来时主动
 * 把焦点拿过来，否则表现为「UI 无法控制」。捕获阶段监听；已有焦点时跳过，
 * 避免每次点击都过一遍 AttachThreadInput。返回解绑函数供卸载时清理。
 */
export function wireClickFocus(label: string): () => void {
  const handler = () => {
    if (document.hasFocus()) return;
    void focusOverlay(label).catch(() => {});
  };
  window.addEventListener("pointerdown", handler, true);
  return () => window.removeEventListener("pointerdown", handler, true);
}
/** 整份覆盖写（Rust 侧会广播，但会覆盖别的窗口改动）：新代码一律用 updateConfig */
export const saveConfig = (config: AppConfig) =>
  invoke("save_config", { config });

/**
 * 配置增量更新的 patch：
 * - settings / widgets：按键合并到磁盘最新配置上，值为 null 表示删除该键
 * - games：整体替换（数组无法按键合并）
 */
export interface ConfigPatch {
  settings?: Record<string, unknown>;
  widgets?: Record<string, boolean>;
  games?: GameItem[];
}

/**
 * 保存配置的唯一入口：按 key 增量写入，Rust 侧在磁盘最新配置上合并后原子落盘并
 * 广播 config://updated，返回合并后的完整配置。
 * 用它替代 saveConfig，可避免「本窗口的旧副本整份覆盖」抹掉其他窗口的改动。
 */
export const updateConfig = (patch: ConfigPatch) =>
  invoke<AppConfig>("update_config", { patch });

/** 导出配置到指定路径（后端会抹掉 API Key 等敏感项，可安全分享） */
export const exportConfig = (path: string) => invoke<void>("export_config", { path });

/** 从文件导入配置（合并语义：文件里没有的键保持本机原样，不会覆盖已填的 API Key） */
export const importConfig = (path: string) => invoke<AppConfig>("import_config", { path });

/** 去掉 Vue 响应式代理，得到可安全序列化的纯数据 */
export const plain = <T>(v: T): T => JSON.parse(JSON.stringify(v)) as T;

const sameValue = (a: unknown, b: unknown) =>
  JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

/**
 * 保存基线：记录上次落盘时的快照，下次保存只提交与基线不同的键。
 * 用于设置中心这类「持有整份配置、改动键很多」的窗口。
 */
export function createConfigDiffer(get: () => AppConfig) {
  let baseSettings: Record<string, unknown> = {};
  let baseWidgets: Record<string, boolean> = {};
  let baseGames = "[]";

  function snapshot() {
    const c = get();
    baseSettings = { ...c.settings };
    baseWidgets = { ...c.widgets };
    baseGames = JSON.stringify(c.games ?? []);
  }

  /** 本地 games 相对基线是否有未保存改动 */
  function gamesDirty() {
    return JSON.stringify(get().games ?? []) !== baseGames;
  }

  function build(): ConfigPatch {
    const c = get();
    const settings: Record<string, unknown> = {};
    const widgets: Record<string, boolean> = {};
    for (const k of new Set([...Object.keys(c.settings), ...Object.keys(baseSettings)])) {
      if (k in c.settings) {
        const cur = c.settings[k];
        if (!sameValue(cur, baseSettings[k])) settings[k] = cur === undefined ? null : cur;
      } else if (k in baseSettings) {
        settings[k] = null; // 本地删掉了这个键
      }
    }
    for (const k of new Set([...Object.keys(c.widgets), ...Object.keys(baseWidgets)])) {
      if (k in c.widgets) {
        if (!sameValue(c.widgets[k], baseWidgets[k])) widgets[k] = c.widgets[k];
      }
    }
    const patch: ConfigPatch = {};
    if (Object.keys(settings).length) patch.settings = settings;
    if (Object.keys(widgets).length) patch.widgets = widgets;
    if (gamesDirty()) patch.games = plain(c.games ?? []);
    return patch;
  }

  function hasChanges() {
    const p = build();
    return !!(p.settings || p.widgets || p.games);
  }

  /** 只更新 games 基线（别的窗口改了 games 时用，避免带着旧副本保存） */
  function snapshotGames() {
    baseGames = JSON.stringify(get().games ?? []);
  }

  return { snapshot, snapshotGames, build, hasChanges, gamesDirty };
}
/**
 * 打开设置中心（Rust 独立线程处理，避免 JS 侧窗口操作与事件循环互等卡死）。
 * 设置窗口常驻隐藏、保留上次浏览的标签页；带意图的入口（如「添加游戏」）
 * 传 tab 先广播 settings://nav，页面切过去之后再显示。齿轮等普通打开不传，
 * 维持「回到上次看的地方」的习惯。
 */
export const openSettingsCenter = (tab?: string) =>
  tab
    ? emit("settings://nav", tab).then(() => invoke("open_settings_center_cmd"))
    : invoke("open_settings_center_cmd");
export const launchProcess = (exePath: string) =>
  invoke("launch_process", { exePath });
export const extractExeIcon = (exePath: string) =>
  invoke<string>("extract_exe_icon", { exePath });
export const scanDirectory = (dir: string) =>
  invoke<ScanResult[]>("scan_directory", { dir });
/** 一键智能扫描：Steam / Epic / GOG / 开始菜单快捷方式 汇总候选 */
export const smartScanGames = () => invoke<ScanResult[]>("smart_scan_games");
/** 攻略助手：截屏 → 视觉大模型识别任务 → 内置浏览器跳 B 站搜攻略（进度走 guide://status） */
export const guideRun = () => invoke("guide_run");
/** 攻略助手：设置中心「测试连接」，返回模型回复文本（失败抛错） */
export const guideCheck = () => invoke<string>("guide_check");
export const listPowerPlans = () =>
  invoke<PowerPlan[]>("list_power_plans");
export const setPowerPlan = (guid: string) =>
  invoke("set_power_plan", { guid });

export interface AudioDevice {
  id: string;
  name: string;
  is_default: boolean;
}

export interface EndpointVolume {
  volume: number; // 0-100
  muted: boolean;
}

export type AudioKind = "output" | "input";

export const listAudioDevices = (kind: AudioKind) =>
  invoke<AudioDevice[]>("list_audio_devices", { kind });
export const setDefaultAudioDevice = (deviceId: string) =>
  invoke("set_default_audio_device", { deviceId });
export const getDeviceVolume = (deviceId: string, kind: AudioKind) =>
  invoke<EndpointVolume>("get_device_volume", { deviceId, kind });
export const setDeviceVolume = (
  deviceId: string,
  kind: AudioKind,
  opts: { volume?: number; mute?: boolean }
) => invoke("set_device_volume", { deviceId, kind, ...opts });

/** 音频会话（音量混合器条目）：pid=0 为系统声音 */
export interface AudioSession {
  pid: number;
  name: string;
  /** exe 完整路径，用于提取图标；系统声音为空串 */
  path: string;
  volume: number; // 0-100
  muted: boolean;
  /** 会话是否正在输出声音 */
  active: boolean;
  system: boolean;
}

export const listAudioSessions = () => invoke<AudioSession[]>("list_audio_sessions");
export const setSessionVolume = (
  pid: number,
  opts: { volume?: number; mute?: boolean }
) => invoke("set_session_volume", { pid, ...opts });

export interface DisplayResolution {
  width: number;
  height: number;
  /** 该分辨率下可用的刷新率，降序 */
  refreshes: number[];
}

export interface DisplayStatus {
  device: string;
  brightness_supported: boolean;
  /** 0-100；读不到当前值时为 null（仍可调节） */
  brightness: number | null;
  resolutions: DisplayResolution[];
  current_width: number;
  current_height: number;
  current_refresh: number;
  /** HDR 开/关；null = 状态读取不可用（仅影响显示，不影响开关操作） */
  hdr_enabled: boolean | null;
}

export const getDisplayStatus = () =>
  invoke<DisplayStatus>("get_display_status");
export const setBrightness = (value: number) =>
  invoke("set_brightness", { value });
export const setDisplayMode = (width: number, height: number, refresh: number) =>
  invoke("set_display_mode", { width, height, refresh });
export const setHdr = (enabled: boolean) =>
  invoke("set_hdr", { enabled });
/** 轻量 HDR 状态探测：返回 [CCD, DXGI]，DXGI 优先采信（悬浮条快捷开关轮询用） */
export const hdrProbe = () =>
  invoke<[boolean | null, boolean | null]>("hdr_probe");
/** 发送 Win+Alt+B（Xbox Game Bar HDR 开关） */
export const toggleHdrHotkey = () => invoke("toggle_hdr_hotkey");

/* ---------------- 麦克风 / 虚拟键盘 ---------------- */

export const getMicMuted = () => invoke<boolean>("get_mic_muted");
/** 切换默认麦克风静音，返回切换后的状态（true=已静音） */
export const toggleMicMute = () => invoke<boolean>("toggle_mic_mute");
export const showVirtualKeyboard = () => invoke("show_virtual_keyboard");

/* ---------------- 无线电：Wi-Fi / 蓝牙 / 飞行模式 ---------------- */

export interface RadioStatus {
  wifi_on: boolean;
  bt_on: boolean;
  airplane: boolean;
}

export const getRadioStatus = () => invoke<RadioStatus>("get_radio_status");
export const setWifiEnabled = (on: boolean) =>
  invoke<boolean>("set_wifi_enabled", { on });
export const setBtEnabled = (on: boolean) =>
  invoke<boolean>("set_bt_enabled", { on });
export const setAirplaneMode = (on: boolean) =>
  invoke<boolean>("set_airplane_mode", { on });

export interface WifiNetwork {
  ssid: string;
  signal: number;
  auth: "open" | "wpa2" | "wpa3" | "wep" | "unknown";
  connected: boolean;
}

export interface WifiConnection {
  connected: boolean;
  ssid: string;
  signal: number;
}

export interface WifiStatus {
  radio_on: boolean;
  connection: WifiConnection;
}

export const getWifiStatus = () => invoke<WifiStatus>("get_wifi_status");
export const listWifiNetworks = () => invoke<WifiNetwork[]>("list_wifi_networks");
export const connectWifi = (ssid: string, password?: string) =>
  invoke("connect_wifi", { ssid, password: password ?? null });
export const disconnectWifi = () => invoke("disconnect_wifi");

/* ---------------- 文件保存地址 / 系统设置 ---------------- */

export const defaultSaveDir = () => invoke<string>("default_save_dir");
export const openInExplorer = (path: string) =>
  invoke("open_in_explorer", { path });
/** page：bluetooth | network | network-wifi | network-airplane-mode */
export const openSystemPage = (page: string) =>
  invoke("open_system_page", { page });

/** 写一行到 Rust 日志（logs/EasyGamingBar.log）：子窗口没有可捞的 console，排查用 */
export function logLine(msg: string, level: "info" | "warn" | "error" = "info") {
  void invoke("debug_log", { msg, level }).catch(() => {});
}

export interface HotkeyBinding {
  action: string;
  key: string;
  registered: boolean;
}

export const getHotkeyBindings = () =>
  invoke<HotkeyBinding[]>("get_hotkey_bindings");
/** 设置自定义唤醒键（如 "Win+Shift+G"）；被占用时返回错误文案 */
export const setUiHotkey = (key: string) =>
  invoke<void>("set_ui_hotkey", { key });
/** 设置任意可重绑的全局快捷键（action: "toggle-ui" | "keyboard" 等）；被占用时返回错误文案 */
export const setActionHotkey = (action: string, key: string) =>
  invoke<void>("set_action_hotkey", { action, key });

/* ---------------- 开机自启 ---------------- */

export const getAutostart = () => invoke<boolean>("get_autostart");
export const setAutostart = (enabled: boolean) =>
  invoke("set_autostart", { enabled });

/* ---------------- 性能监控 ---------------- */

/** 前台全屏游戏信息（monitor 为所在显示器的物理像素矩形 [x, y, w, h]） */
export interface PerfGame {
  pid: number;
  name: string;
  /** 渲染帧率（注入 DLL 在 Present 里量帧时间算出，即游戏真实帧率）；null = 尚未测得 */
  fps: number | null;
  /** 1% Low（最差 1% 帧时间均值折算，PresentMon 同口径） */
  fps_low: number | null;
  monitor: [number, number, number, number];
}

export interface PerfStatus {
  cpu: number | null;
  gpu: number | null;
  ram: number | null;
  /** 显存已用 / 总量（字节） */
  vram_used: number | null;
  vram_total: number | null;
  /** 显存占用百分比 */
  vram: number | null;
  game: PerfGame | null;
}

export const getPerfStatus = () => invoke<PerfStatus>("get_perf_status");
/** 把 FPS 悬浮窗定位到游戏所在显示器指定角落并显示 */
export const placeFpsOverlay = (pos: FpsOverlayPos) =>
  invoke("place_fps_overlay", { pos });

/** FPS 悬浮窗在游戏画面内的位置 */
export type FpsOverlayPos = "tl" | "tr" | "bl" | "br";

/* ---------------- 游戏内覆盖层（DLL 注入） ---------------- */

/**
 * 注入状态：把悬浮条画面直接画进游戏交换链，独占全屏（FSE）下也能看见。
 * 注入有被反作弊识别的风险，所以默认关闭，由用户在设置中心自行开启。
 */
export interface InjectStatus {
  /** 已注入的游戏进程；0 = 当前没有 */
  pid: number;
  name: string;
  /** DLL 成功改写的虚表槽位数（0 = 没挂钩成功） */
  hooked: number;
  /** DLL 状态码：1 正在绘制 / 2 非 D3D11 只计帧 / 3 纹理打不开 / 4 绘制失败 / 5 缺 d3dcompiler */
  status: number;
  /** 已绘制帧数 */
  draws: number;
  /** hook 累计的呈现帧数 */
  frames: number;
  /** 人话说明，直接显示 */
  note: string;
}
export const getInjectStatus = () => invoke<InjectStatus>("get_inject_status");

/* ---------------- 快捷指令 ---------------- */

/** 截图结果：saved=false 表示用户取消/超时未框选 */
export interface ScreenshotResult {
  saved: boolean;
  /** 保存成功时的 PNG 绝对路径 */
  path: string | null;
}

/**
 * 截图：自动隐藏本软件 UI → 系统截图框选（Win+Shift+S）→ 保存 PNG。
 * 保存目录以磁盘配置 settings.saveDir 为权威来源，saveDir 参数仅作
 * 配置缺失时的兜底，最终仍回退系统图片库。
 */
export const quickScreenshot = (saveDir: string) =>
  invoke<ScreenshotResult>("quick_screenshot", { saveDir });
/** 全屏直截：光标所在显示器整屏 PNG，无需框选。保存目录规则同 quickScreenshot */
export const quickScreenshotFull = (saveDir: string) =>
  invoke<ScreenshotResult>("quick_screenshot_full", { saveDir });
/** 打开截图保存目录（不存在则先创建），返回实际目录 */
export const quickOpenScreenshotDir = (saveDir: string) =>
  invoke<string>("quick_open_screenshot_dir", { saveDir });
/** 发送 Win+D，显示桌面（再按还原） */
export const quickShowDesktop = () => invoke("quick_show_desktop");

/** 立即关机（shutdown /s /t 0） */
export const quickShutdown = () => invoke("quick_shutdown");
/** 向前台窗口发送 Alt+F4 等效关闭消息，返回目标窗口标题 */
export const quickCloseGame = () => invoke<string>("quick_close_game");

/** 外部 FPSOverlay（github.com/aneeskhan47/fps-overlay）：启动（需管理员授权）/ 停止 */
export const fpsOverlayLaunch = () => invoke("fps_overlay_launch");
export const fpsOverlayStop = () => invoke("fps_overlay_stop");

export const fpsOverlayPrefs = (config: AppConfig): {
  enabled: boolean;
  pos: FpsOverlayPos;
} => ({
  enabled: config.settings["fpsOverlay"] === true,
  pos: (config.settings["fpsOverlayPos"] as FpsOverlayPos) || "tl",
});
