<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { open, save as saveDialog } from "@tauri-apps/plugin-dialog";
import WIcon from "../shared/WIcon.vue";
import GpSelect from "./GpSelect.vue";
import { applySelfGlass } from "../shared/glass";
import { enterOnShow, notifyReady } from "../shared/enter";
import {
  defaultSaveDir,
  extractExeIcon,
  getAutostart,
  getInjectStatus,
  getHotkeyBindings,
  guideCheck,
  setUiHotkey,
  importSkinImage,
  listAudioDevices,
  createConfigDiffer,
  loadConfig,
  openInExplorer,
  restoreFocus,
  scanDirectory,
  scanSteamGames,
  smartScanGames,
  setAutostart,
  setActionHotkey,
  updateConfig,
  exportConfig,
  importConfig,
  type AppConfig,
  type AudioDevice,
  type GameItem,
  type HotkeyBinding,
  type ScanResult,
  type SteamGame,
} from "../shared/api";
import { WIDGETS, widgetDesc, widgetName } from "../shared/widgets";
import { t, LANGS, setLang, currentLang, type Lang } from "../shared/i18n";
import { BUILTIN_SKINS, readSkin, skinName, skinStyle, type SkinConf } from "../shared/skins";
import { startGamepadNav } from "../shared/gamepad";

const TABS = [
  { id: "games", key: "set.tab.games", icon: "gamepad" },
  { id: "widgets", key: "set.tab.widgets", icon: "cpu" },
  { id: "quicksw", key: "set.tab.quicksw", icon: "sliders" },
  { id: "audio", key: "set.tab.audio", icon: "volume" },
  { id: "general", key: "set.tab.general", icon: "folder" },
  { id: "appearance", key: "set.tab.appearance", icon: "image" },
  { id: "about", key: "set.tab.about", icon: "gear" },
] as const;

const tab = ref<string>("games");
const config = ref<AppConfig>({ settings: {}, games: [], widgets: {} });
const scanning = ref(false);
const scanHits = ref<ScanResult[]>([]);
const scanChecked = ref<Set<string>>(new Set());
const savedFlash = ref(false);

const iconCache = new Map<string, string>();
const iconVersion = ref(0);

function iconSrc(g: GameItem): string | null {
  if (g.iconPath) return convertFileSrc(g.iconPath);
  const hit = iconCache.get(g.exePath);
  return hit ? convertFileSrc(hit) : null;
}

async function ensureIcons() {
  for (const g of config.value.games) {
    if (g.iconPath || iconCache.has(g.exePath) || iconCache.get(g.exePath) === "") continue;
    iconCache.set(g.exePath, "");
    try {
      const path = await extractExeIcon(g.exePath);
      iconCache.set(g.exePath, path);
      g.iconPath = path;
      iconVersion.value++;
    } catch {
      iconCache.delete(g.exePath);
    }
  }
}

const differ = createConfigDiffer(() => config.value);

let timer: ReturnType<typeof setTimeout> | null = null;
/** 提交改动：只发送与上次落盘快照不同的键，避免整份覆盖抹掉别的窗口的改动 */
async function commit() {
  const patch = differ.build();
  differ.snapshot(); // 先更新基线：提交期间的后续改动留给下一轮
  if (!patch.settings && !patch.widgets && !patch.games) return;
  try {
    await updateConfig(patch); // Rust 侧合并后落盘并广播 config://updated
    savedFlash.value = true;
    setTimeout(() => (savedFlash.value = false), 1200);
  } catch (e) {
    console.error(t("err.save"), e);
  }
}

function persist() {
  if (timer) clearTimeout(timer);
  timer = setTimeout(() => void commit(), 250);
}

/* ---------------- 游戏管理 ---------------- */

async function addGame() {
  const picked = await open({
    multiple: true,
    filters: [{ name: t("set.games.programFilter"), extensions: ["exe", "lnk"] }],
  });
  if (!picked) return;
  const paths = Array.isArray(picked) ? picked : [picked];
  for (const p of paths) {
    const name = p.split(/[\\/]/).pop()!.replace(/\.(exe|lnk)$/i, "");
    config.value.games.push({
      id: crypto.randomUUID(),
      name,
      exePath: p,
    });
  }
  await ensureIcons(); // 先补图标，再落盘，避免图标另起一次写入
  persist();
}

async function scanGames() {
  const dir = await open({ directory: true, title: t("set.games.scanDialog") });
  if (!dir) return;
  scanning.value = true;
  scanHits.value = [];
  scanChecked.value = new Set();
  try {
    scanHits.value = await scanDirectory(dir as string);
  } catch (e) {
    console.error(e);
  } finally {
    scanning.value = false;
  }
}

const smartScanning = ref(false);
/** 一键智能扫描：Steam / Epic / GOG / 开始菜单快捷方式，免找文件夹 */
async function scanSmart() {
  smartScanning.value = true;
  scanHits.value = [];
  scanChecked.value = new Set();
  try {
    const hits = await smartScanGames();
    const known = new Set(config.value.games.map((g) => g.exePath.toLowerCase()));
    scanHits.value = hits.filter((h) => !known.has(h.path.toLowerCase()));
  } catch (e) {
    console.error(e);
  } finally {
    smartScanning.value = false;
  }
}

function toggleScanHit(path: string) {
  const s = new Set(scanChecked.value);
  if (s.has(path)) s.delete(path);
  else s.add(path);
  scanChecked.value = s;
}

async function addCheckedHits() {
  for (const hit of scanHits.value) {
    if (!scanChecked.value.has(hit.path)) continue;
    if (config.value.games.some((g) => g.exePath === hit.path)) continue;
    config.value.games.push({
      id: crypto.randomUUID(),
      name: hit.name,
      exePath: hit.path,
      steamAppId: hit.appId || undefined,
      iconPath: hit.icon || undefined,
    });
  }
  scanHits.value = [];
  await ensureIcons();
  persist();
}

function removeGame(i: number) {
  config.value.games.splice(i, 1);
  persist();
}

function moveGame(i: number, dir: -1 | 1) {
  const j = i + dir;
  const list = config.value.games;
  if (j < 0 || j >= list.length) return;
  [list[i], list[j]] = [list[j], list[i]];
  persist();
}

async function changeIcon(g: GameItem) {
  const picked = await open({
    filters: [{ name: t("common.imageFilter"), extensions: ["png", "jpg", "jpeg", "webp", "ico", "bmp"] }],
  });
  if (!picked) return;
  g.iconPath = picked as string;
  iconVersion.value++;
  persist();
}

/* ---------------- Steam 库导入 ---------------- */

const steamHits = ref<SteamGame[]>([]);
const steamChecked = ref<Set<string>>(new Set());
const steamScanning = ref(false);
const steamErr = ref("");

function steamThumb(g: SteamGame): string | null {
  return g.icon ? convertFileSrc(g.icon) : null;
}

function steamAdded(g: SteamGame): boolean {
  return config.value.games.some(
    (x) => x.steamAppId === g.appid || (g.exe !== "" && x.exePath === g.exe)
  );
}

async function scanSteam() {
  steamScanning.value = true;
  steamErr.value = "";
  steamHits.value = [];
  steamChecked.value = new Set();
  try {
    steamHits.value = await scanSteamGames();
  } catch (e) {
    steamErr.value = String(e);
  } finally {
    steamScanning.value = false;
  }
}

function toggleSteamHit(appid: string) {
  // 已添加的行保持可聚焦（手柄导航不跳行），但切换无效
  if (steamHits.value.some((g) => g.appid === appid && steamAdded(g))) return;
  const s = new Set(steamChecked.value);
  if (s.has(appid)) s.delete(appid);
  else s.add(appid);
  steamChecked.value = s;
}

async function addSteamChecked() {
  let added = 0;
  for (const g of steamHits.value) {
    if (!steamChecked.value.has(g.appid) || steamAdded(g)) continue;
    config.value.games.push({
      id: crypto.randomUUID(),
      name: g.name,
      exePath: g.exe,
      steamAppId: g.appid,
      iconPath: g.icon || undefined,
    });
    added++;
  }
  steamHits.value = [];
  if (added) {
    await ensureIcons();
    persist();
  }
}

/* ---------------- 皮肤背景（外观） ---------------- */

const currentSkin = computed(() => readSkin(config.value.settings));
const currentSkinId = computed(() => currentSkin.value?.id ?? "");
const currentSkinName = computed(() => {
  const s = currentSkin.value;
  if (!s) return "";
  if (s.path) return t("set.appear.custom");
  const b = BUILTIN_SKINS.find((x) => x.id === s.id);
  return b ? skinName(b) : "";
});
/** 悬浮条形态预览：与真实悬浮条一致走宽幅补边版 */
const previewStyle = computed(() => skinStyle(config.value.settings, 0, { wide: true }));
/** 设置中心窗口自身的皮肤背景（信息密度高，遮罩加深） */
const shellSkin = computed(() => skinStyle(config.value.settings, 0.18));
const skinOpacity = computed(() => currentSkin.value?.opacity ?? 55);

function selectSkin(id: string) {
  if (id === "none") {
    delete config.value.settings["skin"];
  } else {
    const conf: SkinConf = { id, opacity: currentSkin.value?.opacity };
    config.value.settings["skin"] = conf;
  }
  persist();
}

async function importSkin() {
  const picked = await open({
    filters: [
      { name: t("common.imageFilter"), extensions: ["png", "jpg", "jpeg", "webp", "bmp", "gif", "avif"] },
    ],
  });
  if (!picked) return;
  try {
    const path = await importSkinImage(picked as string);
    const conf: SkinConf = { id: "custom", path, opacity: currentSkin.value?.opacity };
    config.value.settings["skin"] = conf;
    persist();
  } catch (e) {
    console.error("导入皮肤失败", e);
  }
}

function onSkinOpacity(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value);
  const conf: SkinConf = { ...(currentSkin.value ?? { id: "marathon" }), opacity: v };
  config.value.settings["skin"] = conf;
  persist();
}

/* ---------------- 音频设置 ---------------- */

interface AudioFavorites {
  out: string[];
  input: string[];
}

const outDevices = ref<AudioDevice[]>([]);
const inDevices = ref<AudioDevice[]>([]);

function favValue(kind: "out" | "input", idx: number): string {
  const fav = config.value.settings["audioFavorites"] as
    | AudioFavorites
    | undefined;
  return fav?.[kind]?.[idx] ?? "";
}

function setFavValue(kind: "out" | "input", idx: number, id: string) {
  const fav =
    (config.value.settings["audioFavorites"] as AudioFavorites | undefined) ??
    ({} as AudioFavorites);
  const list = [...(fav[kind] ?? [])];
  list[idx] = id;
  config.value.settings["audioFavorites"] = { ...fav, [kind]: list };
  persist();
}

/** 下拉候选（含「未选择」占位项）——自定义下拉组件用，替代原生 select 的 option */
function favOptions(kind: "out" | "input") {
  const devs = kind === "out" ? outDevices.value : inDevices.value;
  return [
    { value: "", label: t("common.select") },
    ...devs.map((d) => ({
      value: d.id,
      label: d.name + (d.is_default ? ` ${t("common.currentDefault")}` : ""),
    })),
  ];
}

async function loadAudioDevices() {
  try {
    [outDevices.value, inDevices.value] = await Promise.all([
      listAudioDevices("output"),
      listAudioDevices("input"),
    ]);
  } catch (e) {
    console.error("枚举音频设备失败", e);
  }
}

/* ---------------- 通用：文件保存地址 ---------------- */

const defaultDir = ref("");

const saveDir = computed(
  () =>
    String(config.value.settings["saveDir"] ?? defaultDir.value ?? "") || t("common.notSet")
);

/** 切换界面语言：写入配置后由 Rust 广播，所有窗口同步刷新 */
function changeLang(id: Lang) {
  void setLang(id);
}

/* ---------------- 通用：启动与热键 ---------------- */

const autostart = ref(false);
const autostartBusy = ref(false);
const uiHotkey = ref("");
const keyboardHotkey = ref("");
const focusHotkey = ref("");
const hotkeyListening = ref<"" | "ui" | "keyboard" | "focus">("");
const hotkeyError = ref("");

async function toggleAutostart() {
  if (autostartBusy.value) return;
  autostartBusy.value = true;
  const next = !autostart.value;
  try {
    await setAutostart(next);
    autostart.value = next;
    savedFlash.value = true;
    setTimeout(() => (savedFlash.value = false), 1200);
  } catch (e) {
    console.error("设置开机自启失败", e);
  } finally {
    autostartBusy.value = false;
  }
}

/** 进入捕获模式：等待用户按下新的全局组合键（target 区分 唤醒键 / 虚拟键盘键 / 呼出鼠标键） */
function startHotkeyCapture(target: "ui" | "keyboard" | "focus") {
  if (hotkeyListening.value) return;
  hotkeyListening.value = target;
  hotkeyError.value = "";
  const onKey = async (e: KeyboardEvent) => {
    e.preventDefault();
    e.stopPropagation();
    // Esc 取消；忽略纯修饰键
    if (e.key === "Escape") {
      stop();
      return;
    }
    if (["Control", "Shift", "Alt", "Meta"].includes(e.key)) return;
    const parts: string[] = [];
    if (e.metaKey) parts.push("Win");
    if (e.ctrlKey) parts.push("Ctrl");
    if (e.altKey) parts.push("Alt");
    if (e.shiftKey) parts.push("Shift");
    parts.push(
      e.key === " " ? "Space" : e.key.length === 1 ? e.key.toUpperCase() : e.key
    );
    const combo = parts.join("+");
    stop();
    const prev =
      target === "ui" ? uiHotkey.value : target === "focus" ? focusHotkey.value : keyboardHotkey.value;
    if (target === "ui") uiHotkey.value = combo;
    else if (target === "focus") focusHotkey.value = combo;
    else keyboardHotkey.value = combo;
    try {
      if (target === "ui") {
        await setUiHotkey(combo);
        config.value.settings["uiHotkey"] = combo;
      } else if (target === "focus") {
        await setActionHotkey("focus-ui", combo);
        config.value.settings["focusHotkey"] = combo;
      } else {
        await setActionHotkey("keyboard", combo);
        config.value.settings["keyboardHotkey"] = combo;
      }
      await commit(); // 立即落盘（含广播），不等防抖
    } catch (err) {
      if (target === "ui") uiHotkey.value = prev;
      else if (target === "focus") focusHotkey.value = prev;
      else keyboardHotkey.value = prev;
      hotkeyError.value = String(err).replace(/^\"|\"$/g, "");
      setTimeout(() => (hotkeyError.value = ""), 5000);
    }
  };
  const stop = () => {
    hotkeyListening.value = "";
    window.removeEventListener("keydown", onKey, true);
  };
  window.addEventListener("keydown", onKey, true);
}

/** 悬浮条快捷开关 / 游戏介绍卡片的显示开关（缺省开启） */
function quickTgl(key: string): boolean {
  return config.value.settings[key] !== false;
}
function toggleQuickSetting(key: string) {
  const cur = quickTgl(key);
  config.value.settings[key] = !cur;
  persist();
}

async function loadGeneralState() {
  autostart.value = await getAutostart().catch(() => false);
  const bindings = await getHotkeyBindings().catch(() => [] as HotkeyBinding[]);
  uiHotkey.value = bindings.find((b) => b.action === "toggle-ui")?.key ?? "Win+Shift+G";
  keyboardHotkey.value = bindings.find((b) => b.action === "keyboard")?.key ?? "Win+Shift+K";
  focusHotkey.value = bindings.find((b) => b.action === "focus-ui")?.key ?? "Ctrl+Alt+F";
}

/* ---------------- 手柄控制 ---------------- */

const gamepadOn = computed(() => config.value.settings["gamepad"] !== false);
function toggleGamepad() {
  config.value.settings["gamepad"] = !gamepadOn.value;
  persist(); // save_config 时 Rust 侧实时启停轮询
}

/* ---------------- 游戏内覆盖层（DLL 注入） ---------------- */

/** 默认关闭：注入 DLL 有被反作弊误判的风险，必须由用户主动开启 */
const injectOn = computed(() => config.value.settings["dllInject"] === true);
const injectNote = ref("");

function toggleInject() {
  config.value.settings["dllInject"] = !injectOn.value;
  persist();
  // Rust 侧每 1s 读一次配置，这里立即拉一次状态让说明文字跟上
  void refreshInject();
}

async function refreshInject() {
  try {
    const s = await getInjectStatus();
    injectNote.value = s.pid > 0 ? `${s.name}（pid ${s.pid}）：${s.note}` : s.note;
  } catch {
    injectNote.value = "";
  }
}

/* ---------------- 呼出即接管焦点 ---------------- */

const summonFocusOn = computed(() => config.value.settings["summonFocus"] !== false);
function toggleSummonFocus() {
  config.value.settings["summonFocus"] = !summonFocusOn.value;
  persist(); // 唤醒键每次按键现读配置，下次呼出即生效
}

async function pickSaveDir() {
  const dir = await open({ directory: true, title: t("set.general.saveDirDialog") });
  if (!dir) return;
  config.value.settings["saveDir"] = dir as string;
  await persist();
}

function resetSaveDir() {
  delete config.value.settings["saveDir"];
  persist();
}

/* ---------------- 语音输入（whisper） ----------------
 * 长按 B 语音控制直播浏览器依赖本地 whisper.cpp：这里选入 whisper-cli
 * 可执行文件与模型文件（ggml-*.bin），存 config.settings.whisperExe/whisperModel，
 * Rust 侧 voice.rs 每次录音前现读。voice_available 命令做「文件存在」校验并回报原因。 */
const voiceStatus = ref<{ available: boolean; reason: string } | null>(null);
const whisperExe = computed(() => String(config.value.settings["whisperExe"] ?? ""));
const whisperModel = computed(() => String(config.value.settings["whisperModel"] ?? ""));

async function refreshVoice() {
  voiceStatus.value = await invoke<{ available: boolean; reason: string }>("voice_available").catch(
    () => null
  );
}

async function pickWhisperExe() {
  const picked = await open({
    title: t("set.general.voice.pickExe"),
    filters: [{ name: "whisper-cli", extensions: ["exe"] }],
  });
  if (typeof picked !== "string") return;
  config.value.settings["whisperExe"] = picked;
  await persist();
  void refreshVoice();
}

async function pickWhisperModel() {
  const picked = await open({
    title: t("set.general.voice.pickModel"),
    filters: [{ name: "whisper model", extensions: ["bin", "gguf"] }],
  });
  if (typeof picked !== "string") return;
  config.value.settings["whisperModel"] = picked;
  await persist();
  void refreshVoice();
}

function resetVoice() {
  delete config.value.settings["whisperExe"];
  delete config.value.settings["whisperModel"];
  persist();
  void refreshVoice();
}

async function openSaveDir() {
  if (saveDir.value && saveDir.value !== t("common.notSet")) {
    await openInExplorer(saveDir.value).catch((e) => console.error(e));
  }
}

/* ---------------- 攻略助手（云端视觉大模型） ----------------
 * 组件面板点一下：截屏 → 这里配置的 OpenAI 兼容接口识别任务 → 内置浏览器搜 B 站攻略。
 * 存 config.settings.guideEnabled/guideProvider/guideBase/guideModel/guideKey，
 * Rust 侧 guide.rs 每次现读。guide_check 命令发一条纯文本消息验证连通性。 */
const GUIDE_PROVIDERS: Record<string, { base: string; model: string }> = {
  kimi: { base: "https://api.moonshot.cn/v1", model: "kimi-latest" },
  qwen: { base: "https://dashscope.aliyuncs.com/compatible-mode/v1", model: "qwen-vl-plus" },
  zhipu: { base: "https://open.bigmodel.cn/api/paas/v4", model: "glm-4v-flash" },
  openai: { base: "https://api.openai.com/v1", model: "gpt-4o-mini" },
  custom: { base: "", model: "" },
};
const guideOn = computed(() => config.value.settings["guideEnabled"] === true);
const guideProvider = computed(() =>
  String(config.value.settings["guideProvider"] ?? "kimi")
);
const guideBase = computed(() => String(config.value.settings["guideBase"] ?? ""));
const guideModel = computed(() => String(config.value.settings["guideModel"] ?? ""));
const guideKey = computed(() => String(config.value.settings["guideKey"] ?? ""));
const guideOptions = computed(() =>
  Object.keys(GUIDE_PROVIDERS).map((v) => ({ value: v, label: t(`set.guide.provider.${v}`) }))
);
const guideTest = ref<{ ok: boolean; msg: string } | null>(null);
const guideTesting = ref(false);

function toggleGuide() {
  config.value.settings["guideEnabled"] = !guideOn.value;
  guideTest.value = null;
  persist();
}

function setGuide(k: string, ev: Event) {
  config.value.settings[k] = (ev.target as HTMLInputElement).value;
  guideTest.value = null;
  persist();
}

/** 选预设：自动填 base/model（自定义保留现值；用户手改过的也会被预设覆盖，重选即回默认） */
function setGuideProvider(p: string) {
  const preset = GUIDE_PROVIDERS[p] ?? GUIDE_PROVIDERS.custom;
  config.value.settings["guideProvider"] = p;
  if (p !== "custom") {
    config.value.settings["guideBase"] = preset.base;
    config.value.settings["guideModel"] = preset.model;
  }
  guideTest.value = null;
  persist();
}

async function testGuide() {
  if (guideTesting.value) return;
  guideTesting.value = true;
  guideTest.value = null;
  try {
    const reply = await guideCheck();
    guideTest.value = { ok: true, msg: t("set.guide.testOk", { reply }) };
  } catch (e) {
    guideTest.value = { ok: false, msg: t("set.guide.testBad") + String(e) };
  } finally {
    guideTesting.value = false;
  }
}

/* ---------------- 配置备份（导出/导入） ---------------- */

const backupBusy = ref(false);
const backupMsg = ref<{ ok: boolean; msg: string } | null>(null);

async function exportConfigFile() {
  if (backupBusy.value) return;
  const path = await saveDialog({
    defaultPath: "easygamingbar-config.json",
    filters: [{ name: "JSON", extensions: ["json"] }],
  });
  if (!path) return;
  backupBusy.value = true;
  backupMsg.value = null;
  try {
    await exportConfig(path);
    backupMsg.value = { ok: true, msg: t("set.backup.exportOk") };
  } catch (e) {
    backupMsg.value = { ok: false, msg: t("set.backup.fail") + String(e) };
  } finally {
    backupBusy.value = false;
  }
}

async function importConfigFile() {
  if (backupBusy.value) return;
  const path = await open({
    multiple: false,
    filters: [{ name: "JSON", extensions: ["json"] }],
  });
  if (!path) return;
  backupBusy.value = true;
  backupMsg.value = null;
  try {
    const merged = await importConfig(path as string);
    config.value = merged;
    differ.snapshot();
    backupMsg.value = { ok: true, msg: t("set.backup.importOk") };
  } catch (e) {
    backupMsg.value = { ok: false, msg: t("set.backup.fail") + String(e) };
  } finally {
    backupBusy.value = false;
  }
}

/* ---------------- 其他 ---------------- */

function widgetEnabled(id: string) {
  return config.value.widgets[id] !== false;
}

function setWidgetEnabled(id: string, on: boolean) {
  config.value.widgets[id] = on;
  persist();
}

/* ---------------- 悬浮条左缘快捷开关的显示开关 ---------------- */

interface QuickSwitches {
  hdr?: boolean;
  ab?: boolean;
}

const quickSwitches = computed<{ hdr: boolean; ab: boolean }>(() => {
  const q = config.value.settings["quickSwitches"] as QuickSwitches | undefined;
  return { hdr: q?.hdr !== false, ab: q?.ab !== false };
});

function setQuickSwitch(key: "hdr" | "ab", on: boolean) {
  const q = (config.value.settings["quickSwitches"] as QuickSwitches | undefined) ?? {};
  config.value.settings["quickSwitches"] = { ...q, [key]: on };
  persist();
}

async function closeSettings() {
  // 先把前台还给游戏 / 上一个窗口再隐藏自己：设置中心关闭时通常正持有前台，
  // 直接 hide 会让 Windows 把前台交给他处（表现为跳到桌面 / 任务视图）
  void restoreFocus().catch(() => {});
  await getCurrentWindow().hide();
}

const rootEl = ref<HTMLElement | null>(null);

onMounted(() => {
  void applySelfGlass(); // 设置中心：Acrylic 优先
  if (rootEl.value) enterOnShow(rootEl.value);
  notifyReady(); // 窗口被销毁重建时，悬浮条等首帧再 show
  // 手柄：B 返回 = 关闭设置中心
  startGamepadNav({ onBack: closeSettings });
});

const unlisten: Array<() => void> = [];

onMounted(async () => {
  config.value = await loadConfig();
  await ensureIcons(); // 先补齐图标：图标是本地补齐，不该被算成「用户改动」
  differ.snapshot(); // 基线：之后保存只提交与基线不同的键
  await loadAudioDevices();
  await loadGeneralState();
  await refreshInject(); // 游戏内覆盖层的注入状态（关闭时也要显示「已关闭」）
  void refreshVoice(); // whisper 语音链路可用性（直播长按 B 语音）
  unlisten.push(
    await listen("inject://status", () => {
      void refreshInject();
    })
  );
  defaultDir.value = await defaultSaveDir().catch(() => "");
  // 别的窗口改了配置（如悬浮条累计游玩时长）：本地 games 无未保存改动时同步为最新，
  // 避免本窗口带着旧副本保存时把那些改动抹掉
  unlisten.push(
    await listen("config://updated", async () => {
      if (differ.gamesDirty()) return;
      try {
        config.value.games = (await loadConfig()).games;
        differ.snapshotGames();
      } catch {
        /* 读取失败保持现状 */
      }
    })
  );
  // 带意图的打开（悬浮条/games flyout 的「添加游戏」）：切到指定标签页再显示。
  // 页面常驻保留上次浏览位置，只有收到导航事件才切；无效 id 忽略。
  unlisten.push(
    await listen<string>("settings://nav", (e) => {
      const id = String(e.payload ?? "");
      if (TABS.some((x) => x.id === id)) tab.value = id;
    })
  );
});

onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
});

</script>

<template>
  <div ref="rootEl" class="shell" :style="shellSkin">
    <header data-tauri-drag-region>
      <span class="logo" data-tauri-drag-region>EasyGamingBar <em>{{ t("set.center") }}</em></span>
      <button class="close" @click="closeSettings"><WIcon name="close" :size="16" /></button>
    </header>

    <div class="content">
      <!-- data-gp-zone：手柄分区导航（shared/gamepad.ts）——左侧导航列与右侧
           内容区各自独立上下移动，内容区左缘按左才回到导航列，避免上下移动时
           焦点被导航列「吸走」（外观页滑杆够不着的根源） -->
      <nav data-gp-zone>
        <button
          v-for="tb in TABS"
          :key="tb.id"
          :class="{ on: tab === tb.id }"
          :data-gp-initial="tab === tb.id ? '' : null"
          @click="tab = tb.id"
        >
          <WIcon :name="tb.icon" :size="17" />
          {{ t(tb.key) }}
        </button>
      </nav>

      <main data-gp-zone>
        <!-- 游戏管理 -->
        <section v-if="tab === 'games'">
          <div class="toolbar">
            <button class="primary" :disabled="smartScanning" @click="scanSmart">
              <WIcon name="zap" :size="15" />
              {{ smartScanning ? t("set.games.smarting") : t("set.games.smart") }}
            </button>
            <button class="ghost" @click="addGame">
              <WIcon name="plus" :size="15" /> {{ t("set.games.add") }}
            </button>
            <button class="ghost" :disabled="scanning" @click="scanGames">
              <WIcon name="search" :size="15" />
              {{ scanning ? t("set.games.scanning") : t("set.games.scan") }}
            </button>
            <button class="ghost" :disabled="steamScanning" @click="scanSteam">
              <WIcon name="steam" :size="15" />
              {{ steamScanning ? t("set.games.steamReading") : t("set.games.steam") }}
            </button>
            <span class="flex" data-flex></span>
            <span class="saved" :class="{ show: savedFlash }">{{ t("common.saved") }}</span>
          </div>

          <div v-if="scanHits.length" class="scanbox">
            <div class="scanhead">
              <span>{{ t("set.games.scanHint", { n: scanHits.length }) }}</span>
              <span class="flex" data-flex></span>
              <button class="ghost" @click="scanHits = []">{{ t("common.cancel") }}</button>
              <button class="primary" @click="addCheckedHits">
                {{ t("set.games.addSelected", { n: scanChecked.size }) }}
              </button>
            </div>
            <div class="scanlist">
              <label v-for="h in scanHits" :key="h.path" class="scanrow">
                <input
                  type="checkbox"
                  :checked="scanChecked.has(h.path)"
                  @change="toggleScanHit(h.path)"
                />
                <span class="sname">{{ h.name }}</span>
                <span class="spath">{{ h.path }}</span>
              </label>
            </div>
          </div>

          <div v-if="steamErr || steamHits.length" class="scanbox steambox">
            <div class="scanhead">
              <template v-if="steamHits.length">
                <span>{{ t("set.games.steamHint", { n: steamHits.length }) }}</span>
                <span class="flex" data-flex></span>
                <button class="ghost" @click="steamHits = []">{{ t("common.cancel") }}</button>
                <button class="primary" @click="addSteamChecked">
                  {{ t("set.games.addSelected", { n: steamChecked.size }) }}
                </button>
              </template>
              <span v-else>{{ steamErr }}</span>
            </div>
            <div class="scanlist">
              <label
                v-for="g in steamHits"
                :key="g.appid"
                class="scanrow"
                :class="{ added: steamAdded(g) }"
              >
                <input
                  type="checkbox"
                  :checked="steamChecked.has(g.appid)"
                  @change="toggleSteamHit(g.appid)"
                />
                <span class="sthumb">
                  <img v-if="steamThumb(g)" :src="steamThumb(g)!" alt="" />
                  <span v-else>{{ g.name.slice(0, 1).toUpperCase() }}</span>
                </span>
                <span class="sname">{{ g.name }}</span>
                <span class="spath">{{ g.exe || t("set.games.viaSteam") }}</span>
                <span v-if="steamAdded(g)" class="tag">{{ t("set.games.added") }}</span>
              </label>
            </div>
          </div>

          <ul class="gamelist">
            <li v-for="(g, i) in config.games" :key="g.id">
              <div class="gicon">
                <img v-if="iconSrc(g) && iconVersion >= 0" :src="iconSrc(g)!" alt="" />
                <span v-else>{{ g.name.slice(0, 1).toUpperCase() }}</span>
              </div>
              <div class="gmeta">
                <div class="gname">{{ g.name }}</div>
                <div class="gpath">{{ g.exePath }}</div>
              </div>
              <div class="gops">
                <button class="mini" :title="t('set.games.customIcon')" @click="changeIcon(g)">
                  <WIcon name="image" :size="14" />
                </button>
                <button class="mini" :title="t('set.games.moveUp')" @click="moveGame(i, -1)">
                  <WIcon name="up" :size="14" />
                </button>
                <button class="mini" :title="t('set.games.moveDown')" @click="moveGame(i, 1)">
                  <WIcon name="down" :size="14" />
                </button>
                <button class="mini danger" :title="t('set.games.delete')" @click="removeGame(i)">
                  <WIcon name="trash" :size="14" />
                </button>
              </div>
            </li>
            <li v-if="config.games.length === 0" class="nonedesc">
              {{ t("set.games.empty") }}
            </li>
          </ul>
        </section>

        <!-- 组件管理 -->
        <section v-else-if="tab === 'widgets'">
          <div class="toolbar">
            <span class="sectitle">{{ t("set.widgets.hint") }}</span>
            <span class="flex" data-flex></span>
            <span class="saved" :class="{ show: savedFlash }">{{ t("common.saved") }}</span>
          </div>
          <ul class="wlist">
            <li v-for="w in WIDGETS" :key="w.id">
              <div class="gmeta">
                <div class="gname">{{ widgetName(w) }}</div>
                <div class="gpath">{{ widgetDesc(w) }}</div>
              </div>
              <span v-if="!w.ready" class="tag">{{ t("set.widgets.soon") }}</span>
              <button
                class="switch"
                :class="{ on: widgetEnabled(w.id) }"
                @click="setWidgetEnabled(w.id, !widgetEnabled(w.id))"
              >
                <span class="knob"></span>
              </button>
            </li>
          </ul>
        </section>

        <!-- 快捷开关 -->
        <section v-else-if="tab === 'quicksw'">
          <div class="toolbar">
            <span class="sectitle">{{ t("set.quicksw.hint") }}</span>
            <span class="flex" data-flex></span>
            <span class="saved" :class="{ show: savedFlash }">{{ t("common.saved") }}</span>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.quicksw.group") }}</div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.quicksw.hdr.name") }}</div>
                <div class="rsub">{{ t("set.quicksw.hdr.desc") }}</div>
              </div>
              <button
                class="switch"
                :class="{ on: quickSwitches.hdr }"
                @click="setQuickSwitch('hdr', !quickSwitches.hdr)"
              >
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.quicksw.ab.name") }}</div>
                <div class="rsub">{{ t("set.quicksw.ab.desc") }}</div>
              </div>
              <button
                class="switch"
                :class="{ on: quickSwitches.ab }"
                @click="setQuickSwitch('ab', !quickSwitches.ab)"
              >
                <span class="knob"></span>
              </button>
            </div>
          </div>
        </section>

        <!-- 音频设置 -->
        <section v-else-if="tab === 'audio'">
          <div class="toolbar">
            <span class="sectitle">{{ t("set.audio.hint") }}</span>
            <span class="flex" data-flex></span>
            <span class="saved" :class="{ show: savedFlash }">{{ t("common.saved") }}</span>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.audio.out") }}</div>
            <!-- 用 div 而非 label：label 会把点击转发给它包裹的控件，
                 里面放按钮会造成「点一下开、转发那下又关」的双触发 -->
            <div class="selrow" v-for="i in 2" :key="'o' + i">
              <span class="sellabel">{{ t("set.audio.slotOut", { i }) }}</span>
              <GpSelect
                :model-value="favValue('out', i - 1)"
                :options="favOptions('out')"
                @update:model-value="setFavValue('out', i - 1, $event)"
              />
            </div>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.audio.in") }}</div>
            <div class="selrow" v-for="i in 2" :key="'i' + i">
              <span class="sellabel">{{ t("set.audio.slotIn", { i }) }}</span>
              <GpSelect
                :model-value="favValue('input', i - 1)"
                :options="favOptions('input')"
                @update:model-value="setFavValue('input', i - 1, $event)"
              />
            </div>
          </div>
        </section>

        <!-- 通用 -->
        <section v-else-if="tab === 'general'">
          <div class="toolbar">
            <span class="sectitle">{{ t("set.general.title") }}</span>
            <span class="flex" data-flex></span>
            <span class="saved" :class="{ show: savedFlash }">{{ t("common.saved") }}</span>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.general.boot") }}</div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.autostart.name") }}</div>
                <div class="rsub">{{ t("set.general.autostart.desc") }}</div>
              </div>
              <button
                class="switch"
                :class="{ on: autostart }"
                :disabled="autostartBusy"
                @click="toggleAutostart"
              >
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.toggleUi.name") }}</div>
                <div class="rsub">{{ t("set.general.toggleUi.desc") }}</div>
              </div>
              <button
                class="keycap editable"
                :class="{ listening: hotkeyListening === 'ui' }"
                :title="hotkeyListening ? t('set.general.escCancel') : t('set.general.clickChange')"
                @click="startHotkeyCapture('ui')"
              >
                {{ hotkeyListening === "ui" ? t("set.general.pressKey") : uiHotkey }}
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.summonFocus.name") }}</div>
                <div class="rsub">{{ t("set.general.summonFocus.desc") }}</div>
              </div>
              <button class="switch" :class="{ on: summonFocusOn }" @click="toggleSummonFocus">
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.keyboard.name") }}</div>
                <div class="rsub">{{ t("set.general.keyboard.desc") }}</div>
              </div>
              <button
                class="keycap editable"
                :class="{ listening: hotkeyListening === 'keyboard' }"
                :title="
                  hotkeyListening ? t('set.general.escCancel') : t('set.general.clickChange2')
                "
                @click="startHotkeyCapture('keyboard')"
              >
                {{ hotkeyListening === "keyboard" ? t("set.general.pressKey") : keyboardHotkey }}
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.focusMouse.name") }}</div>
                <div class="rsub">{{ t("set.general.focusMouse.desc") }}</div>
              </div>
              <button
                class="keycap editable"
                :class="{ listening: hotkeyListening === 'focus' }"
                :title="hotkeyListening ? t('set.general.escCancel') : t('set.general.clickChange2')"
                @click="startHotkeyCapture('focus')"
              >
                {{ hotkeyListening === "focus" ? t("set.general.pressKey") : focusHotkey }}
              </button>
            </div>
            <div v-if="hotkeyError" class="hotkey-err">{{ hotkeyError }}</div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.gamepad.name") }}</div>
                <div class="rsub">{{ t("set.general.gamepad.desc") }}</div>
              </div>
              <button class="switch" :class="{ on: gamepadOn }" @click="toggleGamepad">
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.lang.name") }}</div>
                <div class="rsub">{{ t("set.general.lang.desc") }}</div>
              </div>
              <div class="langops">
                <button
                  v-for="l in LANGS"
                  :key="l.id"
                  class="langbtn"
                  :class="{ on: currentLang() === l.id }"
                  @click="changeLang(l.id)"
                >
                  {{ l.native }}
                </button>
              </div>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.hdr.name") }}</div>
                <div class="rsub">{{ t("set.general.hdr.desc") }}</div>
              </div>
              <button
                class="switch"
                :class="{ on: quickSwitches.hdr }"
                @click="setQuickSwitch('hdr', !quickSwitches.hdr)"
              >
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.ab.name") }}</div>
                <div class="rsub">{{ t("set.general.ab.desc") }}</div>
              </div>
              <button
                class="switch"
                :class="{ on: quickSwitches.ab }"
                @click="setQuickSwitch('ab', !quickSwitches.ab)"
              >
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.introCard.name") }}</div>
                <div class="rsub">{{ t("set.general.introCard.desc") }}</div>
              </div>
              <button
                class="switch"
                :class="{ on: quickTgl('gameIntro') }"
                @click="toggleQuickSetting('gameIntro')"
              >
                <span class="knob"></span>
              </button>
            </div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.general.inject.name") }}</div>
                <div class="rsub">{{ t("set.general.inject.desc") }}</div>
              </div>
              <button class="switch" :class="{ on: injectOn }" @click="toggleInject">
                <span class="knob"></span>
              </button>
            </div>
            <div v-if="injectOn" class="injectnote">
              {{ t("set.general.inject.warn") }}
              <template v-if="injectNote"><br />{{ injectNote }}</template>
            </div>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.general.saveDir") }}</div>
            <div class="dirrow">
              <WIcon name="folder" :size="15" />
              <span class="dirpath" :title="saveDir">{{ saveDir }}</span>
            </div>
            <div class="dirnote">{{ t("set.general.saveDirNote") }}</div>
            <div class="dirops">
              <button class="ghost" @click="pickSaveDir">
                <WIcon name="folder" :size="14" /> {{ t("common.browse") }}
              </button>
              <button class="ghost" @click="openSaveDir">
                <WIcon name="switch" :size="14" /> {{ t("common.openDir") }}
              </button>
              <button
                class="ghost"
                :disabled="config.settings['saveDir'] === undefined"
                @click="resetSaveDir"
              >
                {{ t("common.reset") }}
              </button>
            </div>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.general.voice.name") }}</div>
            <div class="dirrow">
              <WIcon name="mic" :size="15" />
              <span class="dirpath" :title="whisperExe || t('common.notSet')">
                {{ whisperExe || t("common.notSet") }}
              </span>
            </div>
            <div class="dirrow">
              <WIcon name="mic" :size="15" />
              <span class="dirpath" :title="whisperModel || t('common.notSet')">
                {{ whisperModel || t("common.notSet") }}
              </span>
            </div>
            <div class="dirnote">{{ t("set.general.voice.desc") }}</div>
            <div v-if="voiceStatus" class="injectnote" :class="{ ok: voiceStatus.available }">
              {{
                voiceStatus.available
                  ? t("set.general.voice.ok")
                  : t("set.general.voice.bad") + voiceStatus.reason
              }}
            </div>
            <div class="dirops">
              <button class="ghost" @click="pickWhisperExe">
                <WIcon name="folder" :size="14" /> {{ t("set.general.voice.pickExeBtn") }}
              </button>
              <button class="ghost" @click="pickWhisperModel">
                <WIcon name="folder" :size="14" /> {{ t("set.general.voice.pickModelBtn") }}
              </button>
              <button
                class="ghost"
                :disabled="!whisperExe && !whisperModel"
                @click="resetVoice"
              >
                {{ t("common.reset") }}
              </button>
            </div>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.guide.name") }}</div>
            <div class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.guide.enable") }}</div>
                <div class="rsub">{{ t("set.guide.enableDesc") }}</div>
              </div>
              <button class="switch" :class="{ on: guideOn }" @click="toggleGuide">
                <span class="knob"></span>
              </button>
            </div>
            <div class="dirnote">{{ t("set.guide.desc") }}</div>
            <template v-if="guideOn">
              <div class="selrow">
                <span class="sellabel">{{ t("set.guide.providerLabel") }}</span>
                <GpSelect
                  :model-value="guideProvider"
                  :options="guideOptions"
                  @update:model-value="setGuideProvider"
                />
              </div>
              <div class="selrow">
                <span class="sellabel wide">Base URL</span>
                <input
                  class="txtin"
                  type="text"
                  :value="guideBase"
                  :placeholder="t('set.guide.basePh')"
                  spellcheck="false"
                  @input="setGuide('guideBase', $event)"
                />
              </div>
              <div class="selrow">
                <span class="sellabel">{{ t("set.guide.model") }}</span>
                <input
                  class="txtin"
                  type="text"
                  :value="guideModel"
                  :placeholder="t('set.guide.modelPh')"
                  spellcheck="false"
                  @input="setGuide('guideModel', $event)"
                />
              </div>
              <div class="selrow">
                <span class="sellabel wide">API Key</span>
                <input
                  class="txtin"
                  type="password"
                  :value="guideKey"
                  :placeholder="t('set.guide.keyPh')"
                  spellcheck="false"
                  autocomplete="off"
                  @input="setGuide('guideKey', $event)"
                />
              </div>
              <div v-if="guideTest" class="injectnote" :class="{ ok: guideTest.ok }">
                {{ guideTest.msg }}
              </div>
              <div class="dirops">
                <button class="ghost" :disabled="guideTesting" @click="testGuide">
                  {{ guideTesting ? t("set.guide.testing") : t("set.guide.test") }}
                </button>
              </div>
            </template>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.backup.name") }}</div>
            <div class="dirnote">{{ t("set.backup.desc") }}</div>
            <div v-if="backupMsg" class="injectnote" :class="{ ok: backupMsg.ok }">
              {{ backupMsg.msg }}
            </div>
            <div class="dirops">
              <button class="ghost" :disabled="backupBusy" @click="exportConfigFile">
                {{ t("set.backup.export") }}
              </button>
              <button class="ghost" :disabled="backupBusy" @click="importConfigFile">
                {{ t("set.backup.import") }}
              </button>
            </div>
          </div>
        </section>

        <!-- 外观：皮肤背景 -->
        <section v-else-if="tab === 'appearance'">
          <div class="toolbar">
            <span class="sectitle">{{ t("set.appear.hint") }}</span>
            <span class="flex" data-flex></span>
            <span class="saved" :class="{ show: savedFlash }">{{ t("common.saved") }}</span>
          </div>

          <div class="audiogroup">
            <div class="grouptitle">{{ t("set.appear.group") }}</div>
            <div class="skingrid">
              <button class="skincard" :class="{ on: !currentSkin }" @click="selectSkin('none')">
                <span class="skinprev skin-none"><WIcon name="close" :size="16" /></span>
                <span class="skinname">{{ t("set.appear.classic") }}</span>
              </button>
              <button
                v-for="s in BUILTIN_SKINS"
                :key="s.id"
                class="skincard"
                :class="{ on: currentSkinId === s.id }"
                @click="selectSkin(s.id)"
              >
                <span
                  class="skinprev"
                  :style="{
                    backgroundImage: `url(${s.url})`,
                    backgroundColor: s.thumb,
                    backgroundSize: 'contain',
                  }"
                ></span>
                <span class="skinname">{{ skinName(s) }}</span>
              </button>
            </div>
            <div class="skinops">
              <button class="ghost" @click="importSkin">
                <WIcon name="image" :size="14" /> {{ t("set.appear.import") }}
              </button>
              <button v-if="currentSkin" class="ghost" @click="selectSkin('none')">
                {{ t("common.reset") }}
              </button>
              <span v-if="currentSkinName" class="skincur">{{
                t("set.appear.current", { name: currentSkinName })
              }}</span>
            </div>
            <div v-if="currentSkin" class="settingrow">
              <div class="rtext">
                <div class="rname">{{ t("set.appear.opacity.name") }}</div>
                <div class="rsub">{{ t("set.appear.opacity.desc") }}</div>
              </div>
              <input
                class="opcslider"
                type="range"
                min="10"
                max="95"
                :value="skinOpacity"
                @input="onSkinOpacity"
              />
              <span class="opcnum">{{ skinOpacity }}%</span>
            </div>
            <div class="skinpreview">
              <div class="pv-label">{{ t("set.appear.preview") }}</div>
              <div class="pv-bar" :style="previewStyle">
                <span class="pv-tile" /><span class="pv-tile" /><span class="pv-tile" />
                <span class="pv-flex" />
                <span class="pv-seg"><i /><i /><i /></span>
                <span class="pv-clock">21:36</span>
              </div>
            </div>
          </div>
        </section>

        <!-- 关于 -->
        <section v-else class="placeholder">
          <WIcon name="gear" :size="36" />
          <p>EasyGamingBar v0.1.0</p>
          <p class="sub">{{ t("set.about.sub") }}</p>
        </section>
      </main>
    </div>
  </div>
</template>

<style scoped>
.shell {
  width: 100vw;
  height: 100vh;
  display: flex;
  flex-direction: column;
  background: var(--panel-bg-dense);
  border: 1px solid var(--panel-border);
  /* 8px = Win11 DWM 系统圆角半径，与窗口圆角化（glass.rs）对齐 */
  border-radius: 8px;
  overflow: hidden;
  box-shadow: var(--panel-inset), 0 10px 40px rgba(0, 0, 0, 0.5);
  /* 入场：设置中心是预创建的隐藏窗口，等可见后由 enterOnShow 加 .enter 播放 */
  opacity: 0;
  transform: translateY(14px) scale(0.97);
  transition: opacity 0.24s ease, transform 0.34s cubic-bezier(0.3, 1.35, 0.4, 1);
}
.shell.enter {
  opacity: 1;
  transform: none;
}

header {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: 46px;
  padding: 0 10px 0 18px;
  background: rgba(255, 255, 255, 0.04);
  border-bottom: 1px solid rgba(255, 255, 255, 0.07);
  cursor: grab;
}
.logo {
  font-size: 14px;
  font-weight: 700;
  color: #fff;
}
.logo em {
  font-style: normal;
  font-weight: 400;
  color: rgba(255, 255, 255, 0.45);
  margin-left: 8px;
  font-size: 12px;
}
.close {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 30px;
  height: 30px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: rgba(255, 255, 255, 0.6);
  cursor: pointer;
}
.close:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #fff;
}

.content {
  flex: 1;
  display: flex;
  min-height: 0;
}

nav {
  flex: none;
  width: 168px;
  padding: 14px 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  border-right: 1px solid rgba(255, 255, 255, 0.06);
}
nav button {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border: none;
  border-radius: 9px;
  background: transparent;
  color: rgba(255, 255, 255, 0.65);
  font-size: 13px;
  cursor: pointer;
  text-align: left;
}
nav button:hover {
  background: rgba(255, 255, 255, 0.06);
  color: #fff;
}
nav button.on {
  background: var(--accent-faint);
  color: var(--accent-text);
}

main {
  flex: 1;
  min-width: 0;
  overflow-y: auto;
  padding: 16px 20px;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 14px;
}
.sectitle {
  font-size: 13px;
  color: rgba(255, 255, 255, 0.5);
}
.flex {
  flex: 1;
}
.saved {
  font-size: 12px;
  color: #6fdc8c;
  opacity: 0;
  transition: opacity 0.2s ease;
}
.saved.show {
  opacity: 1;
}

button.primary,
button.ghost {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 8px 14px;
  border-radius: 9px;
  font-size: 13px;
  cursor: pointer;
  border: 1px solid transparent;
}
button.primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--accent-contrast);
}
button.primary:hover {
  background: var(--accent-hover);
}
button.ghost {
  background: rgba(255, 255, 255, 0.05);
  border-color: rgba(255, 255, 255, 0.14);
  color: #d6d8db;
}
button.ghost:hover {
  background: rgba(255, 255, 255, 0.1);
}
button:disabled {
  opacity: 0.55;
  cursor: default;
}

.scanbox {
  border: 1px solid var(--accent-mid);
  border-radius: 10px;
  margin-bottom: 14px;
  overflow: hidden;
}
.scanhead {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  background: var(--accent-faint);
  font-size: 12px;
  color: var(--accent-text);
}
.scanlist {
  max-height: 180px;
  overflow-y: auto;
}
.scanrow {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 7px 12px;
  font-size: 12px;
  cursor: pointer;
}
.scanrow:hover {
  background: rgba(255, 255, 255, 0.04);
}
/* 已添加的行：保持可聚焦（手柄逐行导航不跳行），但整体置灰示意不可再勾 */
.scanrow.added {
  cursor: default;
}
.scanrow.added .sname,
.scanrow.added .spath {
  opacity: 0.45;
}
.scanrow.added input[type="checkbox"] {
  opacity: 0.35;
}
.sname {
  color: #e8eaed;
  flex: none;
  width: 140px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.spath {
  color: rgba(255, 255, 255, 0.35);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.gamelist,
.wlist {
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.gamelist li,
.wlist li {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 12px;
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.03);
}
.nonedesc {
  justify-content: center;
  color: rgba(255, 255, 255, 0.35);
  font-size: 13px;
  border-style: dashed !important;
}
.gicon {
  flex: none;
  width: 40px;
  height: 40px;
  border-radius: 10px;
  overflow: hidden;
  background: rgba(255, 255, 255, 0.06);
  display: flex;
  align-items: center;
  justify-content: center;
  font-weight: 600;
  color: #fff;
}
.gicon img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.gmeta {
  flex: 1;
  min-width: 0;
}
.gname {
  font-size: 13px;
  color: #e8eaed;
}
.gpath {
  font-size: 11px;
  color: rgba(255, 255, 255, 0.35);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  margin-top: 2px;
}
.gops {
  flex: none;
  display: flex;
  gap: 6px;
}
.mini {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 7px;
  background: rgba(255, 255, 255, 0.04);
  color: rgba(255, 255, 255, 0.65);
  cursor: pointer;
}
.mini:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #fff;
}
.mini.danger:hover {
  background: rgba(232, 90, 90, 0.18);
  border-color: rgba(232, 90, 90, 0.5);
  color: #ff9c9c;
}

.tag {
  flex: none;
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 99px;
  background: rgba(255, 255, 255, 0.08);
  color: rgba(255, 255, 255, 0.5);
}

.audiogroup {
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.03);
  padding: 12px 14px 14px;
  margin-bottom: 14px;
}
.grouptitle {
  font-size: 13px;
  font-weight: 600;
  color: #e8eaed;
  margin-bottom: 10px;
}

/* 启动与热键行 */
.settingrow {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 6px 0;
}
.settingrow + .settingrow {
  border-top: 1px solid rgba(255, 255, 255, 0.05);
  margin-top: 6px;
  padding-top: 12px;
}
.rtext {
  flex: 1;
  min-width: 0;
}
.rname {
  font-size: 13px;
  color: #e8eaed;
}
.rsub {
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.4);
  margin-top: 2px;
}
.keycap {
  flex: none;
  font-family: inherit;
  font-size: 12px;
  color: var(--accent-text);
  background: var(--accent-faint);
  border: 1px solid var(--accent-mid);
  border-radius: 7px;
  padding: 4px 10px;
  white-space: nowrap;
}
.keycap.editable {
  cursor: pointer;
  transition: background 0.15s ease, border-color 0.15s ease;
}
.keycap.editable:hover {
  background: var(--accent-faint);
  border-color: var(--accent-strong);
}
.keycap.listening {
  color: #ffd28a;
  background: rgba(255, 178, 122, 0.12);
  border-color: rgba(255, 178, 122, 0.55);
  animation: kbd-blink 1.1s ease infinite;
}
@keyframes kbd-blink {
  50% {
    border-color: rgba(255, 178, 122, 0.2);
  }
}

/* 语言切换：两个互斥小按钮，选中态走主题强调色 */
.langops {
  flex: none;
  display: flex;
  gap: 6px;
}
.langbtn {
  font-family: inherit;
  font-size: 12px;
  color: #cfd3da;
  background: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 7px;
  padding: 4px 12px;
  cursor: pointer;
  transition: background 0.15s ease, border-color 0.15s ease, color 0.15s ease;
}
.langbtn:hover {
  background: var(--accent-faint);
  border-color: var(--accent-mid);
}
.langbtn.on {
  color: var(--accent-text);
  background: var(--accent-mid);
  border-color: var(--accent-strong);
}
.hotkey-err {
  font-size: 12px;
  color: #ffb37a;
  padding: 4px 2px 0;
}
.settingrow .switch:disabled {
  opacity: 0.6;
  cursor: wait;
}
.selrow {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 8px;
}
.sellabel {
  flex: none;
  width: 52px;
  font-size: 12px;
  color: rgba(255, 255, 255, 0.5);
}
/* 攻略助手：Base URL 等标签更宽，配一个加长变体 */
.sellabel.wide {
  width: 74px;
}
.txtin {
  flex: 1;
  min-width: 0;
  padding: 8px 10px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 8px;
  background: rgba(255, 255, 255, 0.04);
  color: #e8eaed;
  font-size: 12.5px;
  outline: none;
}
.txtin:focus {
  border-color: var(--accent, #4a72e8);
}
.txtin::placeholder {
  color: rgba(255, 255, 255, 0.28);
}
/* 下拉框已换成自定义 GpSelect 组件（原生 <select> 的弹出层是系统窗口，
   手柄按键进不去）。样式在组件内，这里只保证它在行内伸展。
   注：这里曾有一条 `.selrow select { outline: none }`，它会压掉全局手柄
   焦点环（.gp-focus 特异性更低），是「手柄选不中下拉框」的元凶——已随
   原生 select 一起移除。 */

.dirrow {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 9px 11px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 8px;
  background: rgba(255, 255, 255, 0.04);
  color: #cfd2d6;
}
.dirpath {
  flex: 1;
  min-width: 0;
  font-size: 12.5px;
  color: #e8eaed;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  direction: rtl;
  text-align: left;
}
.dirnote {
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.38);
  margin: 9px 2px;
}
/* 注入功能的风险提示：比普通说明更醒目一点，但不上警告色（不是错误） */
.injectnote {
  font-size: 11.5px;
  line-height: 1.6;
  color: rgba(255, 255, 255, 0.5);
  margin: 2px 2px 10px;
  padding: 8px 10px;
  border-left: 2px solid var(--accent);
  background: rgba(255, 255, 255, 0.04);
  border-radius: 0 6px 6px 0;
}
.injectnote.ok {
  border-left-color: #5ad07a;
  color: rgba(150, 235, 170, 0.85);
}
.dirops {
  display: flex;
  gap: 8px;
}

/* 皮肤背景 */
.skingrid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(122px, 1fr));
  gap: 10px;
}
.skincard {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 0 0 4px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.02);
  cursor: pointer;
  overflow: hidden;
  transition: border-color 0.15s ease;
}
.skincard:hover {
  border-color: rgba(255, 255, 255, 0.32);
}
.skincard.on {
  border-color: var(--accent-hover);
  box-shadow: 0 0 0 1px var(--accent-hover);
}
.skinprev {
  display: block;
  height: 56px;
  background-size: cover;
  background-position: center;
}
.skin-none {
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(33, 35, 38, 0.98);
  color: rgba(255, 255, 255, 0.3);
}
.skinname {
  font-size: 12px;
  color: #d6d8db;
  text-align: center;
}
.skincard.on .skinname {
  color: var(--accent-text);
}
.skinops {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 12px;
}
.skincur {
  font-size: 12px;
  color: rgba(255, 255, 255, 0.45);
}
.opcslider {
  flex: none;
  width: 180px;
  accent-color: var(--accent-hover);
}
.opcnum {
  flex: none;
  width: 40px;
  text-align: right;
  font-size: 12px;
  color: var(--accent-text);
  font-variant-numeric: tabular-nums;
}
.skinpreview {
  margin-top: 14px;
}
.pv-label {
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.4);
  margin-bottom: 6px;
}
.pv-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 52px;
  padding: 0 12px;
  border-radius: 10px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  background: rgba(33, 35, 38, 0.98);
}
.pv-tile {
  width: 34px;
  height: 34px;
  border-radius: 8px;
  background: rgba(255, 255, 255, 0.14);
}
.pv-flex {
  flex: 1;
}
.pv-seg {
  display: flex;
  gap: 6px;
  padding: 4px;
  border-radius: 8px;
  background: rgba(255, 255, 255, 0.08);
}
.pv-seg i {
  width: 20px;
  height: 20px;
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.18);
}
.pv-clock {
  font-size: 15px;
  font-weight: 600;
  color: #f2f3f5;
}

/* Steam 库列表 */
.sthumb {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 30px;
  height: 30px;
  border-radius: 7px;
  overflow: hidden;
  background: rgba(255, 255, 255, 0.08);
  font-size: 13px;
  font-weight: 600;
  color: #fff;
}
.sthumb img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.scanrow .tag {
  flex: none;
}

input[type="checkbox"] {
  accent-color: var(--accent-hover);
}

.switch {
  flex: none;
  position: relative;
  width: 40px;
  height: 22px;
  border: none;
  border-radius: 99px;
  background: rgba(255, 255, 255, 0.14);
  cursor: pointer;
  transition: background 0.15s ease;
}
.switch.on {
  background: var(--accent);
}
.knob {
  position: absolute;
  top: 3px;
  left: 3px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: #fff;
  transition: transform 0.15s ease;
}
.switch.on .knob {
  transform: translateX(18px);
}

.placeholder {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  height: 70%;
  color: rgba(255, 255, 255, 0.4);
  font-size: 14px;
}
.placeholder .sub {
  font-size: 12px;
  color: rgba(255, 255, 255, 0.25);
}

/* 手柄焦点环：gp 导航的可见高亮（无此样式时手柄焦点不可见 =「选不了」） */
:deep(.gp-focus),
.gp-focus {
  outline: 2px solid var(--accent, #4a8fe7);
  outline-offset: 1px;
  border-radius: 6px;
}
</style>
