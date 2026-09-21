<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize, PhysicalPosition, PhysicalSize } from "@tauri-apps/api/dpi";
import { emit, listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import WIcon from "../shared/WIcon.vue";
import QuickToggles from "./QuickToggles.vue";
import { skinStyle } from "../shared/skins";
import { requestGpFocus, startGamepadNav } from "../shared/gamepad";
import { applySelfGlass, setSelfGlass } from "../shared/glass";
import { showWhenReady } from "../shared/enter";
import {
  extractExeIcon,
  focusOverlay,
  launchProcess,
  loadConfig,
  logLine,
  openSettingsCenter as openSettingsCenterCmd,
  plain,
  steamPlaytime,
  trackGameSession,
  updateConfig,
  wireClickFocus,
  type AppConfig,
  type GameItem,
} from "../shared/api";
import { WIDGETS, widgetById, widgetName } from "../shared/widgets";
import { t } from "../shared/i18n";

const BAR_H = 68; // 悬浮条高度（逻辑 px）
const PER_PAGE = 5; // 左侧游戏区每页图标数
const config = ref<AppConfig>({ settings: {}, games: [], widgets: {} });
/** Steam 总时长（appid → 分钟），卡片展示取 Steam 与本地累计的较大值 */
const steamStats = ref<Record<string, { minutes: number; last: number | null }>>({});

function statsFor(g: GameItem) {
  const steam = g.steamAppId ? steamStats.value[g.steamAppId] : undefined;
  return {
    playSeconds: Math.max(steam ? steam.minutes * 60 : 0, g.playSeconds ?? 0),
    lastPlayed: steam?.last ?? g.lastPlayed ?? 0,
    launches: g.launches ?? 0,
  };
}

async function refreshSteamStats() {
  try {
    const ids = config.value.games
      .map((x) => x.steamAppId)
      .filter((x): x is string => !!x);
    if (!ids.length) return;
    const rows = await steamPlaytime(ids);
    steamStats.value = Object.fromEntries(
      rows.map((r) => [r.appid, { minutes: r.minutes, last: r.last_played }])
    );
  } catch {
    /* 忽略 */
  }
}
const openWidgets = ref<Set<string>>(new Set());
const widgetSlots = new Map<string, number>(); // widgetId -> 平铺槽位号
const clock = ref("");

const compact = computed(() => config.value.settings["compactMode"] === true);
const skin = computed(() => skinStyle(config.value.settings, 0, { wide: true }));
const barHLogical = () => (compact.value ? 56 : BAR_H);

/* 折叠态窗口尺寸（逻辑像素）：只比把手（46×13，hover 撑到 16）大一圈。
 * 收起时**必须真的缩小窗口**——透明窗口的鼠标命中测试按整个窗口矩形算，
 * 只把 .bar 用 CSS 翻出视口的话，那条全宽 × 68px 的空窗口仍然挡着下面
 * 网页的点击（用户反馈：收起悬浮条后和悬浮条重叠的网页点不动）。
 * 默认 y 只有 8px，折叠后窗口几乎原地不动，所以挡的就是网页顶部一整条。 */
const HANDLE_W = 54;
const HANDLE_H = 20;

/* ---------------- Tooltip：独立气泡小窗 ----------------
 * 窗口外内容会被裁剪、条内又没空地放气泡（旧方案曾盖住游戏图标；「窗口增高
 * 放气泡条」也会让系统毛玻璃背景板在增高区露出一块黑带），改用一个免焦点、
 * 鼠标穿透的独立小窗贴在悬浮元素下方显示。 */

const tipWinReady = ref(false);
let tipWinTried = false; // 已尝试创建（失败允许下次悬停重试）
let tipActive = false;
let tipHideTimer: ReturnType<typeof setTimeout> | null = null;

async function ensureTipWindow() {
  if (tipWinTried) return;
  tipWinTried = true;
  if (await WebviewWindow.getByLabel("tooltip")) {
    tipWinReady.value = true;
    return;
  }
  const win = new WebviewWindow("tooltip", {
    url: "tooltip.html",
    width: 420,
    height: 32,
    resizable: false,
    decorations: false,
    transparent: true,
    alwaysOnTop: true,
    skipTaskbar: true,
    shadow: false,
    focus: false, // 不抢游戏焦点
    visible: false,
    dragDropEnabled: false,
  });
  win.once("tauri://created", async () => {
    await win.setIgnoreCursorEvents(true); // 纯展示，永不挡游戏点击
    tipWinReady.value = true;
  });
  win.once("tauri://error", (e) => {
    console.error("tooltip 窗口创建失败", e);
    tipWinTried = false;
  });
}

/* 窗口位置 / DPI 缩放缓存：hover 是高频路径，原来每次悬停都要同步 await
   两次 IPC（outerPosition + scaleFactor），而窗口位置只在拖动时才会变。
   拖动 / 缩放变化由 tauri://move 与折叠等入口调用 invalidateWinCache() 失效。 */
let winPosCache: { x: number; y: number } | null = null;
let winPosAt = 0;
let winScaleCache = 0;
const WIN_CACHE_MS = 800;

async function barOuter() {
  const now = performance.now();
  if (winPosCache && now - winPosAt < WIN_CACHE_MS) return winPosCache;
  const p = await getCurrentWindow().outerPosition();
  winPosCache = { x: p.x, y: p.y };
  winPosAt = now;
  return winPosCache;
}

async function barScale() {
  if (winScaleCache) return winScaleCache;
  winScaleCache = await getCurrentWindow().scaleFactor();
  return winScaleCache;
}

function invalidateWinCache() {
  winPosCache = null;
  winPosAt = 0;
}

/** 上一次气泡的「文本 + 落点」指纹：鼠标在同一控件内微动会反复触发 mouseover，
 *  重复 emit 只会让气泡窗口跟着重定位（一次 DWM 重合成 = 肉眼可见的顿挫）。 */
let lastTipKey = "";

async function showTip(text: string, rect: DOMRect, anchorBottom?: number) {
  await ensureTipWindow();
  if (!tipWinReady.value) return;
  const pos = await barOuter();
  const scale = await barScale();
  // 锚点用「整条悬浮条的下沿」而不是元素下沿：元素位于条内偏上时（如左缘的
  // HDR / A|B 文字图标），按元素下沿定位会让气泡落在悬浮条矩形内部，
  // 被悬浮条自身盖住。统一挂到条的下沿外侧，任何元素都不会被遮。
  // 例外：快捷指令抽屉在条下方，气泡要挂到抽屉下沿，否则会盖住那排图标。
  const barEl = document.querySelector(".bar") as HTMLElement | null;
  const anchorY = anchorBottom ?? (barEl ? barEl.getBoundingClientRect().bottom : rect.bottom);
  const x = pos.x + Math.round((rect.left + rect.width / 2) * scale);
  const y = pos.y + Math.round((anchorY + 6) * scale);
  const key = `${text}|${x}|${y}`;
  if (key === lastTipKey) return;
  lastTipKey = key;
  await emit("tip://show", { text, x, y });
  tipActive = true;
}

function scheduleHide() {
  if (!tipActive) return;
  tipActive = false;
  lastTipKey = "";
  if (tipHideTimer) clearTimeout(tipHideTimer);
  tipHideTimer = setTimeout(() => void emit("tip://hide"), 100);
}

/** 游戏介绍卡片开关（设置中心 → 通用；缺省开启）。关闭后悬停/手柄焦点
 *  都不再弹卡片，点击图标始终直接启动。 */
function introCardOn(): boolean {
  return config.value.settings["gameIntro"] !== false;
}

/** 上一次 mouseover 命中的控件（已 closest 到 [data-tip]/[data-intro]）。
 *  鼠标在同一控件内微动会反复触发 mouseover（进入 svg/path 子元素也算一次），
 *  结果完全一样却要重算一遍、并让气泡/卡片窗口重新定位一次——直接拦掉。 */
let lastOverEl: HTMLElement | null = null;

function onBarOver(e: MouseEvent) {
  // 游戏图标优先走介绍卡片（Steam 库页风格），不走文字气泡
  const t = e.target as HTMLElement | null;
  const owner = (t?.closest?.("[data-tip],[data-intro]") as HTMLElement | null) ?? null;
  if (owner === lastOverEl) return;
  lastOverEl = owner;
  const introEl = t?.closest?.("[data-intro]") as HTMLElement | null;
  if (introEl?.dataset.intro) {
    const g = config.value.games.find((x) => x.id === introEl.dataset.intro);
    if (g && introCardOn()) {
      scheduleHide();
      if (introHideTimer) {
        clearTimeout(introHideTimer);
        introHideTimer = null;
      }
      // 已锁定的游戏：鼠标仍在它上面时保持锁定态，别降级成预览态（否则点不到按钮）
      void showIntro(g);
      return;
    }
    // 开关已关闭：落到下面的普通悬停路径（收卡片）；启动只发生在点击时
  }
  scheduleIntroHide();
  const el = t?.closest?.("[data-tip]") as HTMLElement | null;
  if (!el || !el.dataset.tip) {
    scheduleHide();
    return;
  }
  if (tipHideTimer) {
    clearTimeout(tipHideTimer);
    tipHideTimer = null;
  }
  const drawerEl = t?.closest?.(".quick-drawer") as HTMLElement | null;
  void showTip(
    el.dataset.tip,
    el.getBoundingClientRect(),
    drawerEl ? drawerEl.getBoundingClientRect().bottom : undefined
  );
}

/** 鼠标移出整条：清掉 hover 去重标记，保证下次移回同一控件还能正常响应 */
function onBarLeave() {
  lastOverEl = null;
  scheduleHide();
  scheduleIntroHide();
}

/* ---------------- 游戏介绍卡片：贴悬浮条下沿的独立小窗 ----------------
 * Steam 库页风格的横幅卡片（背景大图 + 简介 + 游玩时长），悬停游戏图标时
 * 在 games-seg 正下方展开，免焦点 + 鼠标穿透（纯展示，不挡游戏点击）。 */

const INTRO_GAP = 4; // 与悬浮条下沿的间隙（逻辑 px）
const INTRO_H = 252; // 卡片窗口高度（与创建参数一致，用于判断下方是否放得下）
const introWinReady = ref(false);
let introWin: WebviewWindow | null = null; // 卡片窗口引用（切换鼠标穿透用）
let introWinTried = false; // 已尝试创建（失败允许下次悬停重试）
let introActive = false;
/** 当前卡片展示的游戏 id：同一个图标被反复 mouseover 时用来去重 */
let introShownId: string | null = null;
/** 卡片窗口是否已处于「鼠标穿透」态：避免每次悬停都打一次 Rust 窗口操作 */
let introIgnoreOn = false;
/** 卡片是否已 mount（页面就绪）——未就绪时 emit 会被丢弃，首次点击就白点 */
let introPageReady = false;
let introHideTimer: ReturnType<typeof setTimeout> | null = null;

async function ensureIntroWindow() {
  if (introWinTried) return;
  introWinTried = true;
  const existing = await WebviewWindow.getByLabel("game-intro");
  if (existing) {
    introWin = existing;
    introWinReady.value = true;
    return;
  }
  const win = new WebviewWindow("game-intro", {
    url: "game-intro.html",
    width: 460,
    height: 252,
    resizable: false,
    decorations: false,
    transparent: true,
    alwaysOnTop: true,
    skipTaskbar: true,
    shadow: false,
    focus: false, // 不抢游戏焦点
    visible: false,
    dragDropEnabled: false,
  });
  win.once("tauri://created", async () => {
    await win.setIgnoreCursorEvents(true); // 纯展示，永不挡游戏点击
    introWin = win;
    introWinReady.value = true;
  });
  win.once("tauri://error", (e) => {
    console.error("game-intro 窗口创建失败", e);
    logLine(`game-intro 窗口创建失败：${String(e)}`, "error");
    introWinTried = false;
  });
}

/**
 * 卡片窗口自愈：启动瞬间并发建窗时，WebView2 可能报「无效的窗口句柄」——
 * 窗口句柄在、webview 却是空的，页面永远不会 mount，卡片就永远不显示。
 * 这里销毁重建，并让调用方再补发一次数据。
 */
async function healIntroWindow() {
  const w = introWin ?? (await WebviewWindow.getByLabel("game-intro"));
  try {
    await w?.destroy();
  } catch {
    /* 窗口本就不存在 */
  }
  introWin = null;
  introWinReady.value = false;
  introWinTried = false;
  introPageReady = false;
  introShownId = null;
  introIgnoreOn = false; // 窗口已销毁，穿透状态随之丢失
  await ensureIntroWindow();
}

async function showIntro(g: GameItem) {
  await ensureIntroWindow();
  // 窗口创建是异步事件回调：刚建好那一瞬可能还没置位，等一下再放弃
  for (let i = 0; i < 20 && !introWinReady.value; i++) {
    await new Promise((r) => setTimeout(r, 100));
  }
  if (!introWinReady.value) {
    logLine(`showIntro：卡片窗口未就绪，放弃（游戏=${g.name}）`, "warn");
    return;
  }
  // 同一张卡片已经挂着就不必重发：mouseover 会重复命中同一个图标，
  // 重发会让卡片窗口再定位一次（DWM 重合成）并重新跑入场动画
  if (introActive && introShownId === g.id) return;
  const pos = await barOuter();
  const scale = await barScale();
  // 水平对齐游戏区段中心、顶边贴悬浮条下沿：介绍卡与悬浮条连为一体
  const seg = document.querySelector(".games-seg") as HTMLElement | null;
  const barEl = document.querySelector(".bar") as HTMLElement | null;
  const sr = seg?.getBoundingClientRect();
  const br = barEl?.getBoundingClientRect();
  const top = br?.top ?? 0;
  const bottom = br?.bottom ?? 68;
  const cx = pos.x + Math.round(((sr?.left ?? 0) + (sr?.width ?? 0) / 2) * scale);
  // 默认贴悬浮条下沿；下方放不下（悬浮条被拖到屏幕底部）就翻到条上方，别跑出屏幕
  const need = Math.round((INTRO_H + INTRO_GAP) * scale);
  const barBottom = pos.y + Math.round(bottom * scale);
  const barTop = pos.y + Math.round(top * scale);
  const flip = barBottom + need > Math.round(screen.height * scale) && barTop - need >= 0;
  const y = flip ? barTop - need : barBottom + Math.round(INTRO_GAP * scale);
  // 锁定态要能点「开始游戏」：临时关掉鼠标穿透；预览态必须穿透，不能挡游戏
  const iw = introWin ?? (await WebviewWindow.getByLabel("game-intro"));
  if (iw) {
    introWin = iw;
    // 卡片全程穿透、不做交互：状态不变时不要每次悬停都调一次（这是一次
    // Rust 窗口操作，原来每 hover 一次就打一次，正是动画顿挫的来源之一）
    if (!introIgnoreOn) {
      await iw.setIgnoreCursorEvents(true).catch(() => {});
      introIgnoreOn = true;
    }
  }
  const payload = {
    id: g.id,
    name: g.name,
    steamAppId: g.steamAppId,
    iconPath: g.iconPath,
    playSeconds: statsFor(g).playSeconds,
    lastPlayed: statsFor(g).lastPlayed,
    launches: statsFor(g).launches,
    x: cx,
    y,
  };
  introShownId = g.id;
  await emit("intro://show", payload);
  // 页面可能还没 mount（此时 emit 会被丢弃，表现为「点了没反应」）：
  // 边等边补发；顺带 ping，页面若只是早先错过了握手也会立刻回 ready
  if (!introPageReady) {
    for (let i = 0; i < 8 && !introPageReady; i++) {
      await new Promise((r) => setTimeout(r, 250));
      await emit("intro://show", payload);
      void emit("intro://ping");
    }
  }
  if (!introPageReady) {
    // 两秒还没动静：基本可判定窗口的 webview 没建起来，销毁重建再补发一轮
    await healIntroWindow();
    for (let i = 0; i < 12 && !introPageReady; i++) {
      await new Promise((r) => setTimeout(r, 250));
      await emit("intro://show", payload);
      void emit("intro://ping");
    }
    if (!introPageReady) console.warn("游戏卡片页面始终未上报就绪（窗口可能没加载页面）");
  }
  introActive = true;
}

/** 收起卡片（并恢复穿透，避免残留的可点击窗口挡住游戏） */
function closeIntro() {
  introActive = false;
  introShownId = null;
  if (introHideTimer) {
    clearTimeout(introHideTimer);
    introHideTimer = null;
  }
  void emit("intro://hide");
  if (introWin && !introIgnoreOn) void introWin.setIgnoreCursorEvents(true).catch(() => {});
}

function scheduleIntroHide() {
  if (!introActive) return;
  introActive = false;
  introShownId = null;
  if (introHideTimer) clearTimeout(introHideTimer);
  // 宽限 300ms：鼠标从图标挪到卡片上这段路不算「移开」
  introHideTimer = setTimeout(() => {
    void emit("intro://hide");
    if (introWin && !introIgnoreOn) void introWin.setIgnoreCursorEvents(true).catch(() => {});
  }, 300);
}

/* ---------------- 隐藏模式：折叠把手 ---------------- */

const folded = ref(localStorage.getItem("bar.folded") === "1");
let foldGlassTimer: ReturnType<typeof setTimeout> | null = null;
// 收起悬浮条时被联动收起的组件面板：展开时按此恢复（面板只是收起，不算关闭）
const panelsFolded = ref<string[]>([]);
async function toggleFold() {
  closeQuick(); // 收起悬浮条时一并收起快捷指令抽屉
  folded.value = !folded.value;
  localStorage.setItem("bar.folded", folded.value ? "1" : "0");
  if (folded.value) {
    // 收起：已开的组件面板一起收起（窗口隐藏复用，按钮保持点亮），展开时恢复。
    // 面板隐藏后没有可操作/可框选的对象，手柄所有者与框选状态一并归位。
    const hidden: string[] = [];
    for (const l of PANEL_LABELS) {
      const w = await WebviewWindow.getByLabel(l);
      if (!w) continue;
      if (await w.isVisible().catch(() => false)) {
        await w.hide().catch(() => {});
        hidden.push(l);
      }
    }
    panelsFolded.value = hidden;
    if (frameMode.value || gpOwner.value) {
      frameMode.value = false;
      void setGpOwner(null);
    }
  } else if (panelsFolded.value.length) {
    // 展开：恢复收起时开着的面板
    for (const l of panelsFolded.value) {
      const w = await WebviewWindow.getByLabel(l);
      await w?.show().catch(() => {});
    }
    panelsFolded.value = [];
  }
  if (foldGlassTimer) {
    clearTimeout(foldGlassTimer);
    foldGlassTimer = null;
  }
  if (folded.value) {
    // 折叠：先把窗口挪到屏幕上沿，等 .bar 翻出视口的动画（0.5s）跑完，
    // 再把窗口**真的缩到把手大小**。提前缩会把正在滑出的条体裁掉，滑出动画
    // 就没了；而一直不缩，那条全宽的透明窗口会一直挡着下面网页的点击。
    const win = getCurrentWindow();
    const mon = await currentMonitor();
    const x = (await win.outerPosition()).x;
    await win.setPosition(new PhysicalPosition(x, mon?.position.y ?? 0));
    // 滑出动画结束后撤毛玻璃：窗口区域只剩系统背景板，会露出一条黑带
    foldGlassTimer = setTimeout(() => {
      void setSelfGlass(false);
      void fitBar(); // 缩到把手大小 + 显示器内重新居中
      // 兜底清残留：透明窗口里被腾空的区域可能保留旧帧，
      // 给根元素挂一次合成属性强制整窗重绘
      const de = document.documentElement;
      de.style.transform = "translateZ(0)";
      requestAnimationFrame(() => (de.style.transform = ""));
    }, 540);
  } else {
    // 展开：先把窗口恢复成整条（否则条体从 20px 高的窗口里滑下来会被裁掉），
    // fitBar 会顺便回到记忆位置并重新居中
    await fitBar();
    void applySelfGlass();
  }
}

/* 把手交互：轻点 / 手柄 A 键展开，或按住往下拉展开 */
let handleDownY = 0;
let handleHeld = false;
let handleDragActed = false; // 下拉手势已触发过展开时，吞掉随后的 click 防二次翻转
function onHandleDown(e: PointerEvent) {
  handleHeld = true;
  handleDownY = e.clientY;
  handleDragActed = false;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}
function onHandleUp(e: PointerEvent) {
  if (!handleHeld) return;
  handleHeld = false;
  const dy = e.clientY - handleDownY;
  if (dy > 24) {
    handleDragActed = true; // 下拉到底展开
    toggleFold();
  }
}
function onHandleClick() {
  if (handleDragActed) {
    handleDragActed = false;
    return;
  }
  toggleFold();
}

const enabledWidgets = () =>
  WIDGETS.filter((w) => config.value.widgets[w.id] !== false);

const WIDGET_ICON: Record<string, string> = {
  audio: "volume",
  performance: "monitorAct",
  display: "monitor",
  quick: "zap",
  guide: "search",
};

/* ---------------- 快捷指令 / 首次提示：独立透明小窗 ----------------
 * 两者都不再撑高悬浮条窗口——撑高会让毛玻璃背景板在新增区域露出一块黑底
 * （与折叠态的黑带同源），而频繁切玻璃又有与窗口线程互等卡死的风险。
 * 改用 tooltip / game-intro 那套独立透明小窗：无装饰、透明、置顶、不进
 * 任务栏，贴在被点图标正下方，视觉上就是「竖排透明图标浮在下面」。 */

const QUICK_WIN = "quick-drawer";
const quickExpanded = ref(false); // 抽屉是否展开（中段按钮点亮态，纯镜像：真相是窗口可见性）
let quickWinReady = false;
let quickWinTried = false;
let quickWinCreating: Promise<boolean> | null = null;

/** 确保抽屉窗口存在。返回 false = 创建失败（本轮不再重试，窗口销毁后重置）。 */
async function ensureQuickWindow(): Promise<boolean> {
  if (await WebviewWindow.getByLabel(QUICK_WIN)) {
    quickWinReady = true;
    return true;
  }
  if (quickWinCreating) return quickWinCreating;
  if (quickWinTried) return quickWinReady;
  quickWinTried = true;
  quickWinCreating = new Promise<boolean>((resolve) => {
    const win = new WebviewWindow(QUICK_WIN, {
      url: "quick-drawer.html",
      width: 60,
      height: 220,
      resizable: false,
      decorations: false,
      transparent: true,
      alwaysOnTop: true,
      skipTaskbar: true,
      shadow: false,
      focus: false, // 不抢游戏焦点；点图标时由系统自然激活
      visible: false,
      dragDropEnabled: false,
    });
    win.once("tauri://created", () => {
      quickWinReady = true;
      resolve(true);
    });
    win.once("tauri://error", (e) => {
      console.error("快捷指令窗口创建失败", e);
      logLine(`快捷指令窗口创建失败：${String(e)}`, "error");
      quickWinTried = false;
      resolve(false);
    });
    // 窗口销毁后必须复位标记：否则 ready=true 但窗口已不在，
    // showQuick 会把 quick://show 发进虚空，抽屉从此「打不开」
    win.once("tauri://destroyed", () => {
      quickWinReady = false;
      quickWinTried = false;
      if (quickExpanded.value) quickExpanded.value = false;
    });
  });
  try {
    return await quickWinCreating;
  } finally {
    quickWinCreating = null;
  }
}

/** 锚点：把元素中心 / 下沿换算成物理坐标，小窗自行居中定位 */
async function anchorBelow(el: HTMLElement | null | undefined) {
  const win = getCurrentWindow();
  const pos = await win.outerPosition();
  const scale = await win.scaleFactor();
  const r = el?.getBoundingClientRect();
  const barEl = document.querySelector(".bar") as HTMLElement | null;
  const cx = r ? r.left + r.width / 2 : (barEl?.offsetWidth ?? 0) / 2;
  const bottom = r ? r.bottom : 68;
  return {
    x: pos.x + Math.round(cx * scale),
    y: pos.y + Math.round((bottom + 8) * scale),
  };
}

async function showQuick(mode: "actions" | "tip") {
  const ok = await ensureQuickWindow();
  if (!ok) {
    logLine(`showQuick：快捷指令窗口未就绪，放弃（mode=${mode}）`, "warn");
    return;
  }
  const barEl = document.querySelector(".bar") as HTMLElement | null;
  const btn = barEl?.querySelector<HTMLElement>(".quick-btn");
  const p = await anchorBelow(btn);
  await emit("quick://show", { mode, x: p.x, y: p.y });
}

async function toggleQuick() {
  // 以窗口真实可见性为准，而不是 quickExpanded 镜像——隐藏复用模式下窗口会被
  // 多处直接 hide（唤醒键隐藏 UI / 看门狗重载 bar 复位镜像），镜像滞后时
  // 第一次点击会被当成「关闭」吞掉，表现为「快捷指令打不开了」
  const existing = await WebviewWindow.getByLabel(QUICK_WIN);
  const vis = await existing?.isVisible().catch(() => false);
  if (vis) {
    quickExpanded.value = false;
    await emit("quick://hide");
    return;
  }
  await showQuick("actions");
  quickExpanded.value = true;
}

async function closeQuick() {
  if (!quickExpanded.value) return;
  quickExpanded.value = false;
  await emit("quick://hide");
}

/* 5 个指令动作（截图 / 返回桌面 / 关机 / 强关游戏 / 看截图）已迁到
 * src/quickdrawer/App.vue：抽屉是窗口，动作与按住进度都在那边实现。 */

/* 关机「按住 1 秒」的进度逻辑同样在 src/quickdrawer/App.vue 里 */

/* ---------------- 首次启动提示（只告知，绝不修改用户设置） ----------------
 * Steam 的「Steam 输入」会抢走 Xbox 手柄输入，导致手柄控不了本软件。
 * 这里只在首次启动时提示一次用户自行到 Steam 里关闭，本软件不动 Steam 配置。 */

const STEAM_TIP_KEY = "bar.steamTipV1";

/** 只提示一次、绝不修改用户设置；卡片本体在 quick-drawer 窗口的 tip 模式里 */
async function maybeShowFirstTip() {
  if (localStorage.getItem(STEAM_TIP_KEY)) return;
  localStorage.setItem(STEAM_TIP_KEY, "1");
  await showQuick("tip");
}

/* ---------------- 首次启动：语言选择 ----------------
 * settings.lang 未落过值才弹一次（独立居中窗口，选完写入配置后 Rust 广播，
 * 所有窗口同步刷新）。localStorage 只做「本次安装弹过一次」的兜底：
 * 用户直接关掉也不至于每次配置变更都再弹出来。 */

const LANG_PICK_KEY = "bar.langPickerV1";

async function openFirstRun() {
  const existing = await WebviewWindow.getByLabel("first-run");
  if (existing) {
    await existing.setFocus();
    return;
  }
  const wL = 420;
  const hL = 320;
  const win = new WebviewWindow("first-run", {
    url: "first-run.html",
    title: "EasyGamingBar",
    width: wL,
    height: hL,
    // 注意：mon.position/size 是物理像素，而 x/y 选项要逻辑像素，混算会在
    // 带缩放的屏上偏到右下（实测向导不在屏幕正中）。交给原生 center 落位
    center: true,
    resizable: false,
    decorations: false,
    // 不透明：release 包以管理员运行时「透明 + WebView2」的命中测试偶发失效，
    // 表现为向导看得见但按钮点不动（实测卡死在选语言）。向导不需要透底，
    // 实底深色彻底绕开该组合，损失的圆角可接受
    transparent: false,
    backgroundColor: "#141518",
    alwaysOnTop: true,
    skipTaskbar: true,
    shadow: true,
    focus: true, // 首启向导需要焦点：此时一般不在游戏中
    visible: false,
    dragDropEnabled: false,
  });
  showWhenReady(win, "first-run"); // 首帧后再显示，避免白框
  win.once("tauri://error", (e) => console.error("语言选择窗口创建失败", e));
}

async function maybeShowLangPicker() {
  // 以磁盘为唯一真相现读一次：config.value 是启动时的内存副本，若那次 load_config
  // 偶发失败（被并发写/占用），副本里没有 lang → 向导误弹出，用户被迫「再选一次」
  // 且后续写入可能覆盖丢设置。磁盘上已有 lang 就绝不弹
  try {
    const fresh = await loadConfig();
    if (fresh?.settings && fresh.settings["lang"] !== undefined) {
      config.value.settings["lang"] = fresh.settings["lang"];
      return;
    }
  } catch {
    /* 读失败走原判定 */
  }
  if (config.value.settings["lang"] !== undefined) return;
  if (localStorage.getItem(LANG_PICK_KEY)) return;
  localStorage.setItem(LANG_PICK_KEY, "1");
  await openFirstRun();
}

/** 语言已在别处（设置中心）选定 → 关掉可能还开着的向导窗 */
async function closeFirstRunIfDone() {
  if (config.value.settings["lang"] === undefined) return;
  const win = await WebviewWindow.getByLabel("first-run");
  if (win) await win.close();
}

/* ---------------- 配置加载 / 持久化 ---------------- */

let saveTimer: ReturnType<typeof setTimeout> | null = null;
/** 悬浮条只写 games（启动次数/最近游玩/累计时长/图标）：按 games 增量写，不整份覆盖别的窗口的 settings */
function persist() {
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    try {
      await updateConfig({ games: plain(config.value.games) }); // Rust 侧落盘后广播 config://updated
    } catch (e) {
      console.error("保存配置失败", e);
    }
  }, 250);
}

async function loadAndApply() {
  config.value = await loadConfig();
  ensureIcons();
  fitBar();
  void reconcileWidgets();
  void refreshSteamStats();
}

/** 配置与组件窗口调和：设置里禁用的组件立即关闭窗口，杜绝「已取消显示却突然出现」 */
async function reconcileWidgets() {
  for (const w of WIDGETS) {
    if (w.id === "quick") continue; // 快捷指令走抽屉窗口，不受组件开关管
    const enabled = config.value.widgets[w.id] !== false;
    const label = `widget-${w.id}`;
    const win = await WebviewWindow.getByLabel(label).catch(() => null);
    if (!win) {
      widgetSlots.delete(w.id);
      openWidgets.value.delete(w.id);
      continue;
    }
    if (!enabled) {
      await win.close().catch(() => {});
      openWidgets.value.delete(w.id);
      widgetSlots.delete(w.id);
      onPanelGone(label);
    }
  }
}

const iconCache = new Map<string, string>();
const iconVersion = ref(0);

function iconSrc(g: GameItem): string | null {
  if (g.iconPath) return convertFileSrc(g.iconPath);
  const hit = iconCache.get(g.exePath);
  return hit ? convertFileSrc(hit) : null;
}

/** 为未配置图标的游戏异步提取 exe 图标（Rust 侧有磁盘缓存） */
async function ensureIcons() {
  for (const g of config.value.games) {
    // 旧版提取的是 32px 小图（路径无 _256 标记）：视为过期，重新提取高清图
    if (g.iconPath?.includes("_256") || iconCache.has(g.exePath)) continue;
    iconCache.set(g.exePath, ""); // 占位防止重复请求
    try {
      const path = await extractExeIcon(g.exePath);
      iconCache.set(g.exePath, path);
      g.iconPath = path;
      iconVersion.value++;
      persist();
    } catch {
      iconCache.delete(g.exePath);
    }
  }
}

/* ---------------- 左侧：游戏图标翻页（每页 3 个，滑动/滚轮/按钮切换） ---------------- */

const pages = computed<GameItem[][]>(() => {
  const out: GameItem[][] = [];
  const gs = config.value.games;
  for (let i = 0; i < gs.length; i += PER_PAGE) out.push(gs.slice(i, i + PER_PAGE));
  return out;
});
const pageCount = computed(() => Math.max(1, pages.value.length));
const page = ref(0);
const dragX = ref(0);
const dragging = ref(false);

watch(pageCount, (n) => {
  if (page.value > n - 1) page.value = Math.max(0, n - 1);
});

// 翻页后，介绍卡锚定的图标可能已被藏进非当前页（不可见）：立即收起卡片，
// 否则卡片挂着「看不见的游戏」悬在组件区上（实机反馈：第 6 个应用显示出来了）
watch(page, () => {
  if (introActive) closeIntro();
});

/** 环回翻页：末页左滑回到初始 3 个图标 */
function stepPage(dir: 1 | -1) {
  const n = pageCount.value;
  if (n < 2) return;
  page.value = (page.value + dir + n) % n;
}

const trackStyle = computed(() => ({
  transform: `translate3d(calc(${-page.value * 100}% + ${dragX.value}px), 0, 0)`,
  transition: dragging.value ? "none" : undefined,
}));

let suppressClickUntil = 0;
function onTileClick(g: GameItem) {
  if (performance.now() < suppressClickUntil) return;
  // 左键 / 手柄 A 键：直接启动，不做多余操作（介绍卡仅悬停/手柄焦点预览）
  void launchGame(g);
}
const pagerEl = ref<HTMLElement | null>(null);
let swipeStartX = 0;
let swipeStartT = 0;
let pointerId = -1;

let pagerCaptured = false; // 捕获会把 click 重定向到容器：只有真滑动才捕获，否则图标点不开

function onPagerDown(e: PointerEvent) {
  if (pageCount.value < 2) return;
  pointerId = e.pointerId;
  swipeStartX = e.clientX;
  swipeStartT = performance.now();
  dragging.value = true;
  pagerCaptured = false;
}

function onPagerMove(e: PointerEvent) {
  if (!dragging.value || e.pointerId !== pointerId) return;
  dragX.value = e.clientX - swipeStartX;
  // 滑动超过阈值才捕获指针，保证静止点击能落到游戏图标上
  if (!pagerCaptured && Math.abs(dragX.value) > 6) {
    pagerEl.value?.setPointerCapture(e.pointerId);
    pagerCaptured = true;
  }
}

function onPagerUp(e: PointerEvent) {
  if (!dragging.value || e.pointerId !== pointerId) return;
  dragging.value = false;
  pagerCaptured = false;
  const dx = e.clientX - swipeStartX;
  const dt = performance.now() - swipeStartT;
  dragX.value = 0;
  if (Math.abs(dx) > 6) suppressClickUntil = performance.now() + 260;
  const w = pagerEl.value?.clientWidth ?? 150;
  const flick = Math.abs(dx) > 28 && dt < 280;
  if (dx < -w * 0.22 || (flick && dx < 0)) stepPage(1);
  else if (dx > w * 0.22 || (flick && dx > 0)) stepPage(-1);
}

let wheelLock = 0;
function onPagerWheel(e: WheelEvent) {
  if (pageCount.value < 2) return;
  e.stopPropagation();
  const now = performance.now();
  if (now - wheelLock < 180 || Math.abs(e.deltaY) < 6) return;
  wheelLock = now;
  stepPage(e.deltaY > 0 ? 1 : -1);
}

/* 点击启动：图标先做一个「弹起 + 光环」脉冲，再真正拉起进程 */
const launchingId = ref<string | null>(null);
let launchTimer: ReturnType<typeof setTimeout> | null = null;

async function launchGame(g: GameItem) {
  launchingId.value = g.id;
  if (launchTimer) clearTimeout(launchTimer);
  launchTimer = setTimeout(() => (launchingId.value = null), 640);
  // 本地游玩统计：启动次数 / 最近游玩立即记录；时长由 Rust 跟踪进程广播增量
  g.launches = (g.launches ?? 0) + 1;
  g.lastPlayed = Math.floor(Date.now() / 1000);
  persist();
  void trackGameSession(g.exePath).catch(() => {});
  try {
    // Steam 游戏走官方协议启动，保证 DRM 与 Steam 运行时正常
    await launchProcess(g.steamAppId ? `steam://rungameid/${g.steamAppId}` : g.exePath);
    g.launches = (g.launches ?? 0) + 1;
    g.lastPlayed = Math.floor(Date.now() / 1000);
    persist();
  } catch (e) {
    console.error("启动失败", e);
  }
}

/* ---------------- 手柄框选两级导航（仲裁状态机在本窗口） ----------------
 * 框选层：主UI整体 + 每个已开组件面板 = 一个「框」，全部主题色描边包围，
 * 方向键在框之间移动选择（焦点框高亮），A 确认进入该界面（元素层）。
 * 元素层：普通手柄焦点导航，且只能操作当前进入的这一个界面。
 * 转移规则（用户定义）：
 *  - 面板内 B → 不关面板，回框选层并框选整个主UI；再按 A 才能操作主UI（仅主UI）；
 *  - 主UI元素层移到最左侧再往左（或未进入过元素层时按方向）→ 进框选层，
 *    再移动方向即可选中某个面板框，A 进入该面板；
 *  - 输入路由由 Rust GAMEPAD_OWNER 落地：框选层/主UI元素层 owner=bar，
 *    面板元素层 owner=widget-<id>；其余窗口收不到手柄输入。 */

type Dir = "up" | "down" | "left" | "right";
const PANEL_LABELS = ["widget-audio", "widget-performance", "widget-display"];

const frameMode = ref(false); // 框选层激活中（主UI与面板整体描边）
const frameFocus = ref("bar"); // 框选层当前选中的框（"bar" 或 widget-* 窗口 label）
const gpOwner = ref<string | null>(null); // 手柄输入所有者（元素层驻留窗口）
let gpEl: HTMLElement | null = null; // 主UI元素层当前焦点元素（判断最左缘用）

const gpTier = computed<"frame" | "element" | "idle">(() =>
  frameMode.value ? "frame" : gpOwner.value ? "element" : "idle"
);

/** 广播层级状态：面板窗口据此显示/切换自己的描边样式 */
function broadcastTier() {
  void emit("gp://tier", {
    mode: gpTier.value,
    owner: gpOwner.value,
    focus: frameMode.value ? frameFocus.value : null,
  });
}

/** 更新手柄输入所有者（Rust 路由据 GAMEPAD_OWNER 独占分流） */
async function setGpOwner(label: string | null) {
  gpOwner.value = label;
  try {
    await invoke("gamepad_set_owner", { label });
  } catch (e) {
    console.error("设置手柄所有者失败", e);
  }
  broadcastTier();
}

/** 进入框选层：先框选整个主UI（用户规则：从面板返回 / 从主UI退出都落到这） */
async function enterFrameMode() {
  frameMode.value = true;
  frameFocus.value = "bar";
  await setGpOwner("bar");
}

/** 框选层按 A：进入框选中的界面（bar = 主UI元素层；widget = 该面板元素层） */
async function confirmFrame() {
  if (!frameMode.value) {
    void logLine("confirmFrame: 不在框选层，忽略 A");
    return;
  }
  const target = frameFocus.value;
  void logLine(`confirmFrame: A 进入 ${target}`);
  if (target === "bar") {
    frameMode.value = false;
    await setGpOwner("bar"); // 只能对主UI操作
    return;
  }
  const win = await WebviewWindow.getByLabel(target);
  if (!win) {
    // 框已不存在（面板刚被关）：退回主UI框
    frameFocus.value = "bar";
    broadcastTier();
    return;
  }
  // 先交归属再显示：路由立刻跟着走。显示/取焦点一律丢后台——
  // JS 侧窗口操作与窗口线程互等会卡住，await 挡住的话后面的归属切换就执行不到
  // （表现为「框选层按 A 没反应」）。取焦点走 Rust 的 focus_overlay（NOACTIVATE
  // 浮窗必须经 AttachThreadInput，直接 setFocus 不可靠）。
  frameMode.value = false;
  await setGpOwner(target);
  void logLine(`confirmFrame: 归属已交 ${target}`);
  void win.show().catch(() => {});
  void focusOverlay(target).catch(() => {});
}

/** 框选层按 B：收起描边、交还手柄（面板保持原样），方向键会再次进入框选层 */
async function dismissFrame() {
  frameMode.value = false;
  await setGpOwner(null);
  // 取焦点丢后台（同上：JS 侧窗口操作别 await，会与窗口线程互等）
  void focusOverlay("bar").catch(() => {});
}

let frameMoving = false;
/** 框选层方向移动：在「主UI + 各已见面板」的窗口矩形间做空间选择（物理像素） */
async function frameStep(dir: Dir) {
  if (frameMoving) return;
  frameMoving = true;
  try {
    const labels = ["bar", ...PANEL_LABELS];
    const rects: Array<{ label: string; x: number; y: number; w: number; h: number }> = [];
    for (const l of labels) {
      const win = l === "bar" ? getCurrentWindow() : await WebviewWindow.getByLabel(l);
      if (!win) continue;
      if (!(await win.isVisible().catch(() => false))) continue;
      try {
        const p = await win.outerPosition();
        const s = await win.outerSize();
        rects.push({ label: l, x: p.x, y: p.y, w: s.width, h: s.height });
      } catch {
        /* 窗口刚销毁 */
      }
    }
    const cur = rects.find((r) => r.label === frameFocus.value) ?? rects[0];
    if (!cur) return;
    if (cur.label !== frameFocus.value) {
      frameFocus.value = cur.label;
      broadcastTier();
    }
    const cx = cur.x + cur.w / 2;
    const cy = cur.y + cur.h / 2;
    let best: string | null = null;
    let bestScore = Infinity;
    for (const r of rects) {
      if (r.label === cur.label) continue;
      const ex = r.x + r.w / 2 - cx;
      const ey = r.y + r.h / 2 - cy;
      let primary: number;
      let cross: number;
      if (dir === "left") {
        if (ex > -10) continue;
        primary = -ex;
        cross = Math.abs(ey);
      } else if (dir === "right") {
        if (ex < 10) continue;
        primary = ex;
        cross = Math.abs(ey);
      } else if (dir === "up") {
        if (ey > -10) continue;
        primary = -ey;
        cross = Math.abs(ex);
      } else {
        if (ey < 10) continue;
        primary = ey;
        cross = Math.abs(ex);
      }
      const score = primary + cross * 1.6;
      if (score < bestScore) {
        bestScore = score;
        best = r.label;
      }
    }
    if (best && best !== frameFocus.value) {
      frameFocus.value = best;
      broadcastTier();
    }
  } finally {
    frameMoving = false;
  }
}

/** 主UI元素层焦点是否已在本窗口最左缘（继续往左 = 退出到框选层） */
function atLeftmost(el: HTMLElement): boolean {
  let min = Infinity;
  document
    .querySelectorAll<HTMLElement>(
      "button:not(:disabled), [data-nav], input:not(:disabled), select:not(:disabled)"
    )
    .forEach((n) => {
      if (n.checkVisibility?.() === false) return;
      const r = n.getBoundingClientRect();
      if (r.width > 0 && r.left < min) min = r.left;
    });
  return Number.isFinite(min) && el.getBoundingClientRect().left <= min + 8;
}

/** 面板被隐藏（面板×关闭 / 悬浮条再点开关 / 设置里禁用）后的所有者清理 */
function onPanelGone(label: string) {
  // 按钮点亮态镜像同步：面板已不可见，中段按钮不该保持点亮
  openWidgets.value.delete(label.replace("widget-", ""));
  if (gpOwner.value === label) void setGpOwner(null);
  if (frameMode.value && frameFocus.value === label) {
    frameFocus.value = "bar";
    broadcastTier();
  }
}

/* ---------------- 中段：组件开关（屏幕左侧平铺，可拖动） ---------------- */

async function toggleWidget(id: string) {
  // 快捷指令已改为内联展开（悬浮条下方出图标），不再开独立组件窗口
  if (id === "quick") {
    void toggleQuick();
    return;
  }
  const label = `widget-${id}`;
  const existing = await WebviewWindow.getByLabel(label);
  if (existing) {
    // 以窗口真实可见性为准（openWidgets 只是按钮态镜像）：
    // 面板自己的 B 键/返回也会隐藏窗口，集合可能滞后
    const vis = await existing.isVisible().catch(() => false);
    if (vis) {
      // 隐藏复用而非销毁：重开瞬时，页面与数据常驻
      await existing.hide();
      openWidgets.value.delete(id);
      onPanelGone(label);
    } else {
      await existing.show();
      openWidgets.value.add(id);
      // 手柄跟随进入该面板（元素层）：只能对这一个面板操作
      frameMode.value = false;
      // 取焦点 / 交手柄归属都是 Rust 窗口操作，await 会把刚开始的点亮动画
      // （on-pop）卡在半路 —— 表现就是「点了按钮，动画先顿一下」。改为后台跑。
      void existing.setFocus();
      void setGpOwner(label);
    }
    return;
  }

  const def = widgetById(id);
  if (!def) return;
  const slot = nextFreeSlot();
  widgetSlots.set(id, slot);
  openWidgets.value.add(id);

  const pos = await slotPosition(slot, def.width, def.height);
  const win = new WebviewWindow(label, {
    url: `widget.html?id=${id}`,
    title: widgetName(def),
    width: def.width,
    height: def.height,
    x: pos.x,
    y: pos.y,
    resizable: false,
    decorations: false,
    transparent: true,
    alwaysOnTop: true,
    skipTaskbar: true,
    shadow: false,
    visible: false,
    dragDropEnabled: false,
  });
  // 等子窗口首帧绘制完成再显示：避免白框闪现；显示后面板自己弹入
  showWhenReady(win, label);
  // 新面板即手柄所有者：与旧的「面板可见即独占」行为一致（游戏手柄 A 打开后立即可操作）
  void (async () => {
    for (let i = 0; i < 40 && !(await win.isVisible().catch(() => false)); i++) {
      await new Promise((r) => setTimeout(r, 50));
    }
    if (openWidgets.value.has(id)) {
      frameMode.value = false;
      void setGpOwner(label);
    }
  })();
  win.once("tauri://error", (e) => {
    console.error("组件窗口创建失败", e);
    openWidgets.value.delete(id);
    widgetSlots.delete(id);
  });
  win.once("tauri://destroyed", () => {
    openWidgets.value.delete(id);
    widgetSlots.delete(id);
    onPanelGone(label);
  });
}

function nextFreeSlot(): number {
  const used = new Set(widgetSlots.values());
  let s = 0;
  while (used.has(s)) s++;
  return s;
}

/** 槽位 → 屏幕左上角平铺坐标（列优先：先上下排满一列，再向右开新列） */
async function slotPosition(slot: number, w: number, h: number) {
  const mon = await currentMonitor();
  const monWL = (mon?.size.width ?? 1920) / (mon?.scaleFactor ?? 1);
  const monHL = (mon?.size.height ?? 1080) / (mon?.scaleFactor ?? 1);

  const gap = 12;
  const x0 = 12;
  const y0 = BAR_H + 12; // 从悬浮条下方开始铺
  const strideY = h + gap;
  const strideX = w + gap;
  const rowsPerCol = Math.max(1, Math.floor((monHL - y0 - gap) / strideY));
  const col = Math.floor(slot / rowsPerCol);
  const row = slot % rowsPerCol;
  const x = x0 + col * strideX > monWL - w ? x0 : x0 + col * strideX;
  const y = y0 + row * strideY;
  return { x: Math.round(x), y: Math.round(y) };
}

/* ---------------- 右侧：时钟 / 鼠标 / 设置下拉 ---------------- */

function tick() {
  const d = new Date();
  clock.value = `${String(d.getHours()).padStart(2, "0")}:${String(
    d.getMinutes()
  ).padStart(2, "0")}`;
}

/* ---- 看直播（画中画浏览器）：开窗 / 锁定 / 解锁 三态 ---- */

const liveOpen = ref(false); // 工具条窗口存在
const liveBrowser = ref(false); // 浏览器内容窗口存在
const liveLocked = ref(false); // 锁定 = 鼠标点击穿透

const liveTip = computed(() => {
  if (!liveOpen.value) return t("bar.live.pick");
  if (liveLocked.value) return t("bar.live.unlock");
  return liveBrowser.value ? t("bar.live.lock") : t("bar.live.choose");
});

async function openLiveToolbar() {
  const existing = await WebviewWindow.getByLabel("live-toolbar");
  if (existing) {
    await existing.show();
    await existing.setFocus();
    liveOpen.value = true;
    return;
  }
  const win = new WebviewWindow("live-toolbar", {
    url: "live-toolbar.html",
    title: t("win.live"),
    width: 480, // 初始宽仅占位：工具条首帧前按内容自适应（见 live/App.vue fitPickerWidth）
    height: 44,
    resizable: false,
    decorations: false,
    transparent: true,
    alwaysOnTop: true,
    skipTaskbar: true,
    shadow: false,
    focus: false, // 不抢游戏焦点
    visible: false,
    dragDropEnabled: false,
  });
  win.once("tauri://created", () => {
    // 定位由工具条页面自己在首帧前完成（内容宽自适应 + 居中）：
    // 这里若再按初始宽居中会晚于它执行，把窗口拽偏出屏
    liveOpen.value = true;
  });
  showWhenReady(win, "live-toolbar", { focus: false }); // 不抢游戏焦点
  win.once("tauri://error", (e) => console.error("直播工具条创建失败", e));
  win.once("tauri://destroyed", () => {
    liveOpen.value = false;
    liveBrowser.value = false;
    liveLocked.value = false;
  });
}

async function toggleLive() {
  if (!liveOpen.value) {
    await openLiveToolbar();
    return;
  }
  if (liveLocked.value) {
    await emit("live://set-locked", false);
    return;
  }
  if (liveBrowser.value) {
    await emit("live://set-locked", true);
    return;
  }
  // 平台选择态再点一次 = 关闭工具条
  liveOpen.value = false;
  const w = await WebviewWindow.getByLabel("live-toolbar");
  await w?.close().catch(() => {});
}

/* ---- 弹出式窗口（设置下拉 / 游戏九宫格），失焦自动关闭 ---- */

/** flyout 布局算法：创建与「隐藏复用重定位」共用——设置面板居中于所在
 *  显示器且不超出屏幕；游戏九宫格贴悬浮条左侧区段。x/y 为物理坐标。 */
async function flyoutPosition(kind: "settings" | "games") {
  const def = kind === "settings" ? { w: 360, h: 640 } : { w: 440, h: 440 };
  const bar = getCurrentWindow();
  const bPos = await bar.outerPosition();
  const scale = await bar.scaleFactor();
  let wL = def.w;
  let hL = def.h;
  let xL: number;
  let yL: number;
  let baseX = bPos.x;
  let baseY = bPos.y;
  let posScale = scale;
  if (kind === "settings") {
    const mon = await currentMonitor();
    posScale = mon?.scaleFactor ?? scale;
    const monWL = (mon?.size.width ?? 1920) / posScale;
    const monHL = (mon?.size.height ?? 1080) / posScale;
    wL = Math.min(def.w, Math.max(280, monWL - 24));
    hL = Math.min(def.h, Math.max(320, monHL - 24));
    xL = Math.round((monWL - wL) / 2);
    yL = Math.round((monHL - hL) / 2);
    baseX = mon?.position.x ?? bPos.x;
    baseY = mon?.position.y ?? bPos.y;
  } else {
    xL = 6;
    yL = BAR_H + 8;
  }
  return {
    x: Math.round(baseX + xL * posScale),
    y: Math.round(baseY + yL * posScale),
    pw: Math.round(wL * posScale),
    ph: Math.round(hL * posScale),
    wL,
    hL,
  };
}

async function openFlyout(kind: "settings" | "games") {
  const label = `flyout-${kind}`;
  const existing = await WebviewWindow.getByLabel(label);
  if (existing) {
    const vis = await existing.isVisible().catch(() => false);
    if (vis) {
      await existing.hide();
      return;
    }
    // 隐藏复用：按当前悬浮条/显示器重新落位后直接显示（页面常驻，秒开）
    const pos = await flyoutPosition(kind);
    await existing.setPosition(new PhysicalPosition(pos.x, pos.y));
    await existing.setSize(new PhysicalSize(pos.pw, pos.ph));
    await existing.show();
    await existing.setFocus();
    return;
  }

  const { wL, hL, x, y } = await flyoutPosition(kind);
  const win = new WebviewWindow(label, {
    url: kind === "settings" ? "flyout-settings.html" : "flyout-games.html",
    title: kind === "settings" ? t("win.flyoutSettings") : t("win.flyoutGames"),
    width: wL,
    height: hL,
    x,
    y,
    resizable: false,
    decorations: false,
    transparent: true,
    alwaysOnTop: true,
    skipTaskbar: true,
    shadow: false,
    focus: true,
    visible: false,
    dragDropEnabled: false,
  });
  showWhenReady(win, label); // 首帧后再显示，避免白框
  win.once("tauri://error", (e) => console.error("flyout 创建失败", e));
}

async function openSettingsCenter(tab?: string) {
  // 「更多设置」：完整设置中心窗口——打开统一走 Rust 命令（独立线程），
  // 避免 JS 侧窗口 show/创建与窗口线程互等造成卡死
  try {
    await openSettingsCenterCmd(tab);
  } catch (e) {
    console.error("打开设置中心失败", e);
  }
}

/* ---------------- 宽度自适应内容并保持顶部居中 ---------------- */

let savedY: number | null = null;

async function fitBar() {
  await nextTick();
  const el = document.querySelector(".bar") as HTMLElement | null;
  if (!el) return;
  // offsetWidth = 布局宽度，不受入场动画 scale 影响（getBoundingClientRect
  // 会取到缩放中的宽度，窗口从此比条窄，右侧被裁一块）。
  // 折叠态窗口只有把手那么大，但 .bar 是 width:max-content，仍能量到真实条宽。
  const w = el.offsetWidth;
  const win = getCurrentWindow();
  // 折叠态：窗口缩到把手大小，屏幕上沿只留这一小块吃鼠标
  await win.setSize(
    folded.value
      ? new LogicalSize(HANDLE_W, HANDLE_H)
      : new LogicalSize(w, barHLogical())
  );
  // 始终在悬浮条所在显示器内水平居中（monitor.position 是虚拟桌面坐标）
  const scale = await win.scaleFactor();
  const mon = await currentMonitor();
  const size = await win.outerSize();
  const baseX = mon?.position.x ?? 0;
  const monW = mon?.size.width ?? size.width;
  const x = Math.round(baseX + (monW - size.width) / 2);
  // 折叠态窗口贴显示器上沿，展开态回到记忆位置
  const y = folded.value ? mon?.position.y ?? 0 : safeSavedY() ?? Math.round(8 * scale);
  await win.setPosition(new PhysicalPosition(x, y));
  invalidateWinCache(); // 窗口刚被重新定位过，缓存作废
}

/* ---------------- 位置记忆 ---------------- */

let moveTimer: ReturnType<typeof setTimeout> | null = null;

/**
 * 坐标是否可信。窗口最小化时 Windows 会把它挪到 (-32000,-32000)，
 * 若这种坐标被记进 localStorage，下次恢复 / 折叠展开就会把悬浮条丢到屏幕外
 * ——表现就是「UI 不见了」。记位置与读位置两头都要挡掉。
 */
function posSane(x: number, y: number): boolean {
  return (
    Number.isFinite(x) && Number.isFinite(y) && x > -8000 && y > -8000 && x < 30000 && y < 30000
  );
}

/** 记忆的 y：非法（含最小化留下的 -32000）就当没记过 */
function safeSavedY(): number | null {
  return savedY != null && posSane(0, savedY) ? savedY : null;
}

async function restorePosition() {
  const saved = localStorage.getItem("bar.pos");
  if (!saved) return;
  try {
    const { y } = JSON.parse(saved) as { y: number };
    if (typeof y === "number" && posSane(0, y)) {
      savedY = y;
    } else {
      savedY = null;
      localStorage.removeItem("bar.pos");
    }
  } catch {
    localStorage.removeItem("bar.pos");
  }
}

function rememberPosition() {
  if (folded.value) return; // 折叠贴边时的移动不记入悬浮条位置
  if (moveTimer) clearTimeout(moveTimer);
  moveTimer = setTimeout(async () => {
    const pos = await getCurrentWindow().outerPosition();
    if (!posSane(pos.x, pos.y)) return; // 最小化等异常坐标不入档
    localStorage.setItem("bar.pos", JSON.stringify({ x: pos.x, y: pos.y }));
  }, 400);
}

/* ---------------- 生命周期 ---------------- */

let unlisten: Array<() => void> = [];
let offClickFocus: (() => void) | null = null;
let clockTimer: ReturnType<typeof setInterval> | null = null;

onMounted(async () => {
  logLine("悬浮条页面已挂载");
  // 覆盖模式：窗口默认是「不激活」地浮在游戏画面上（不抢游戏的键盘焦点），
  // 鼠标真正点进来时才把焦点拿过来，之后键鼠/手柄导航才正常
  offClickFocus = wireClickFocus("bar");
  // 折叠态启动时不铺毛玻璃：条已移出窗口，只剩背景板会露出一条黑带
  if (!folded.value) void applySelfGlass();
  // 预创建小窗。必须串行 + 间隔：同帧连开多个隐藏窗时 WebView2 会偶发
  // 「无效的窗口句柄」(0x80070578)，窗口建出来但 webview 是空的，页面永不上线。
  void (async () => {
    await ensureTipWindow();
    await new Promise((r) => setTimeout(r, 300));
    await ensureIntroWindow();
    await new Promise((r) => setTimeout(r, 300));
    await ensureQuickWindow();
    const has = async (l: string) => !!(await WebviewWindow.getByLabel(l));
    logLine(
      `预创建完成：tooltip=${await has("tooltip")} game-intro=${await has(
        "game-intro"
      )} quick-drawer=${await has(QUICK_WIN)}`
    );
  })();
  tick();
  clockTimer = setInterval(tick, 10_000);
  // 手柄：B 返回 = 收起快捷指令抽屉 / 折叠悬浮条（A 确认 / 摇杆选择由共享导航处理）
  // 折叠态不整体禁用手柄：条体已移出视口，空间收集天然只剩「拉手」一个目标，
  // 手柄 A 键可以把它拉下来（否则折叠后手柄失去目标，再也无法展开）
  unlisten.push(
    startGamepadNav({
      // 框选层激活时元素层导航整体让位（方向/A/B 由 onFrame* 接管）
      isEnabled: () => !frameMode.value,
      frameActive: () => frameMode.value,
      onFrameDir: (dir) => void frameStep(dir),
      onFrameA: () => void confirmFrame(),
      onFrameB: () => void dismissFrame(),
      onBack: () => {
        // B 键：有面板开着 → 回框选层（面板保留，框选整个主UI）；
        // 否则先收快捷指令抽屉，没有抽屉时才收起整条悬浮条
        if (openWidgets.value.size > 0) {
          void enterFrameMode();
          return;
        }
        if (quickExpanded.value) {
          closeQuick();
          return;
        }
        if (!folded.value) void toggleFold();
      },
      onDir: (dir) => {
        // 有面板开着时区分两态（gpOwner 是「是否已进入主UI元素层」的标记）：
        // ① 尚未进入（idle，owner=null）→ 任何方向先进框选层，A 确认后才能操作主UI；
        // ② 已在主UI元素层（owner=bar）→ 方向正常做元素导航，
        //    仅在最左缘继续往左时退出到框选层（用户规则：最左缘进入框选）
        if (openWidgets.value.size === 0) return false;
        if (gpOwner.value !== "bar") {
          void enterFrameMode();
          return true;
        }
        if (dir === "left" && gpEl && atLeftmost(gpEl)) {
          void enterFrameMode();
          return true;
        }
        return false;
      },
      // 手柄没有 hover：焦点落到游戏图标上就展示卡片，移开即收（A 键仍直接启动）
      onFocus: (el) => {
        gpEl = el;
        const host = el?.closest?.("[data-intro]") as HTMLElement | null;
        const id = host?.dataset.intro;
        if (!id) {
          if (introActive) scheduleIntroHide();
          return;
        }
        if (!introCardOn()) return; // 介绍卡片已关：焦点预览也不弹
        const g = config.value.games.find((x) => x.id === id);
        if (!g) return;
        if (introHideTimer) {
          clearTimeout(introHideTimer);
          introHideTimer = null;
        }
        void showIntro(g);
      },
    })
  );
  await restorePosition();
  await loadAndApply();
  void maybeShowLangPicker(); // 首次启动选语言（只弹一次，选完写入配置）
  void maybeShowFirstTip(); // 首次启动的 Steam 手柄提示（只提示一次）
  unlisten.push(
    await listen("config://updated", () => {
      void loadAndApply();
      void closeFirstRunIfDone();
    })
  );
  // 抽屉小窗自己关闭时（点图标/知道了）同步中段按钮的点亮态
  unlisten.push(await listen("quick://closed", () => { quickExpanded.value = false; }));
  /* ---- 输入设备自动切换：鼠标 ↔ 手柄 ----
   * 手柄侧是「锁定」模型（gpOwner / frameMode 独占路由），鼠标一动就该解锁，
   * 否则手柄握着归属、鼠标再怎么点都像没反应；手柄一有输入再锁回去。 */
  unlisten.push(
    await listen("input://mouse", () => {
      // 不管有没有锁，都告诉 Rust 现在是鼠标模式（手柄一动它会自己切回手柄）
      void invoke("set_input_mode", { mode: "mouse" }).catch(() => {});
      if (!frameMode.value && gpOwner.value === null) return;
      frameMode.value = false;
      if (gpOwner.value !== null) void setGpOwner(null);
      else broadcastTier(); // 只为同步面板描边（owner 本就为空）
    })
  );
  unlisten.push(
    await listen<{ label?: string }>("input://gamepad", (e) => {
      const label = e.payload?.label;
      if (!label || frameMode.value) return; // 框选层由状态机自己管，别抢
      if (label === "bar") {
        // 无面板开着时才锁主UI：有面板时留给框选状态机（先按方向进框选层，A 再进面板）
        if (openWidgets.value.size === 0 && gpOwner.value !== "bar") void setGpOwner("bar");
        return;
      }
      // 手柄已经归别处（抽屉 / 设置中心 / 面板）：本窗口不该再挂着焦点环，
      // 否则会出现「两个界面同时亮着焦点环」、看着像手柄同时控制了两边
      requestGpFocus(null);
      // 手柄输入落到了某个面板（焦点路由）→ 归属锁到它，之后不再依赖焦点
      if (label.startsWith("widget-") && gpOwner.value !== label) void setGpOwner(label);
    })
  );
  // 托盘 / 唤醒键「显示界面」：窗口被 show 了，但折叠态下内容翻在窗口外，
  // 看着就是没显示——这里补一次展开（未折叠则忽略）
  unlisten.push(
    await listen("bar://reveal", () => {
      if (folded.value) void toggleFold();
    })
  );
  // 卡片页面就绪握手：未就绪前 showIntro 会等，避免首帧数据被丢弃
  const markReady = () => {
    introPageReady = true;
  };
  unlisten.push(await listen("intro://ready", markReady));
  unlisten.push(
    await listen("intro://boot", () => {
      markReady();
      console.log("游戏卡片页面已加载");
    })
  );
  void emit("intro://ping"); // 卡片页面若已 mount 会立刻回一次 ready
  // 锁定态卡片：鼠标在卡片上时不算「移开」，离开卡片才收
  unlisten.push(
    await listen<{ inside: boolean }>("intro://hover", (e) => {
      if (e.payload?.inside) {
        if (introHideTimer) {
          clearTimeout(introHideTimer);
          introHideTimer = null;
        }
        introActive = true;
      } else {
        scheduleIntroHide();
      }
    })
  );
  // 卡片上的「开始游戏」：启动逻辑在悬浮条这边
  unlisten.push(
    await listen<{ id?: string }>("intro://launch", (e) => {
      const g = config.value.games.find((x) => x.id === e.payload?.id);
      closeIntro();
      if (g) launchGame(g);
    })
  );
  // 游玩时长增量（Rust 后台跟踪游戏进程）：合并进对应游戏并持久化
  unlisten.push(
    await listen<{ exePath: string; addedSecs: number; finished: boolean }>(
      "game://play-progress",
      (e) => {
        const p = e.payload;
        if (!p?.exePath || !p.addedSecs) return;
        const g = config.value.games.find((x) => x.exePath === p.exePath);
        if (!g) return;
        g.playSeconds = (g.playSeconds ?? 0) + p.addedSecs;
        g.lastPlayed = Math.floor(Date.now() / 1000);
        persist();
      }
    )
  );
  // 直播工具条广播的状态（浏览器窗口是否存在 / 是否锁定）
  unlisten.push(
    await listen<{ browser: boolean; locked: boolean }>("live://state", (e) => {
      liveBrowser.value = e.payload?.browser === true;
      liveLocked.value = e.payload?.locked === true;
      if (liveBrowser.value || liveLocked.value) liveOpen.value = true;
    })
  );
  // 面板内按 B：不关面板，回框选层并框选整个主UI（两级导航模型）
  unlisten.push(await listen("gp://leave", () => void enterFrameMode()));
  // 面板自己关闭（× 按钮）：清理手柄所有者/框选焦点
  unlisten.push(
    await listen<{ label: string }>("gp://closed", (e) => {
      if (e.payload?.label) onPanelGone(e.payload.label);
    })
  );
  // 页面重载后重建面板镜像：openWidgets 是内存态，面板窗口本身还开着
  // （HMR / 崩溃自动重载），不同步会让框选层永远不触发
  void (async () => {
    for (const l of PANEL_LABELS) {
      const w = await WebviewWindow.getByLabel(l);
      if (w && (await w.isVisible().catch(() => false))) {
        openWidgets.value.add(l.replace("widget-", ""));
      }
    }
  })();
  // 窗口一动，位置缓存就失效（气泡/卡片都靠它定位）
  const offMoved = await getCurrentWindow().onMoved(() => {
    invalidateWinCache();
    rememberPosition();
  });
  unlisten.push(offMoved);
  window.addEventListener("wheel", (e) => e.preventDefault(), { passive: false });
  // 测试辅助：index.html?demo=<组件id> 启动时自动打开对应组件
  const demo = new URLSearchParams(window.location.search).get("demo");
  if (demo) toggleWidget(demo);
});

onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
  if (clockTimer) clearInterval(clockTimer);
  offClickFocus?.();
});
</script>

<template>
  <!-- 折叠把手：悬浮条全部收起后，屏幕上沿只留这条细把手，轻点 / 下拉展开 -->
  <div
    class="fold-handle"
    :class="{ show: folded }"
    data-nav
    role="button"
    @pointerdown="onHandleDown"
    @pointerup="onHandleUp"
    @click="onHandleClick"
  >
    <WIcon name="down" :size="9" />
  </div>
  <div
    class="bar"
    :class="{
      compact,
      folded,
      'frame-focal': frameMode && frameFocus === 'bar',
      'frame-dim': frameMode && frameFocus !== 'bar',
    }"
    :style="skin"
    data-tauri-drag-region
    @mouseover="onBarOver"
    @mouseleave="onBarLeave()"
  >

    <!-- 左缘快捷开关：HDR + 音频 A/B（上下排列，位于最左游戏图标左侧） -->
    <QuickToggles />

    <!-- 左区段：游戏图标翻页（每页 5 个） + 全部游戏 -->
    <section class="seg games-seg" data-tauri-drag-region>
      <div
        v-if="pages.length"
        ref="pagerEl"
        class="pager"
        @pointerdown="onPagerDown"
        @pointermove="onPagerMove"
        @pointerup="onPagerUp"
        @pointercancel="onPagerUp"
        @wheel="onPagerWheel"
      >
        <div class="track" :style="trackStyle">
          <div
            v-for="(pg, pi) in pages"
            :key="pi"
            class="page"
            :class="{ off: pi !== page && !dragging }"
          >
            <div
              v-for="g in pg"
              :key="g.id"
              class="tile"
              data-nav
              :class="{ launching: launchingId === g.id }"
              :data-intro="g.id"
              @click="onTileClick(g)"
            >
              <img
                v-if="iconSrc(g) && iconVersion >= 0"
                :src="iconSrc(g)!"
                alt=""
                draggable="false"
              />
              <span v-else class="ph">{{
                g.name.slice(0, 1).toUpperCase()
              }}</span>
            </div>
          </div>
        </div>
      </div>
      <div v-else class="game-tile">
        <div class="tile empty" :data-tip="t('bar.addGame')" @click="openSettingsCenter('games')">
          <WIcon name="plus" :size="18" />
        </div>
      </div>
      <button class="icon-btn" :data-tip="t('bar.allGames')" @click="openFlyout('games')">
        <WIcon name="grid" :size="20" />
      </button>
    </section>

    <!-- 中区段：组件开关 -->
    <section class="seg widgets-seg" data-tauri-drag-region>
      <button
        v-for="w in enabledWidgets()"
        :key="w.id"
        class="icon-btn"
        :class="{
          'quick-btn': w.id === 'quick',
          on: w.id === 'quick' ? quickExpanded : openWidgets.has(w.id),
        }"
        :data-tip="widgetName(w)"
        @click="w.id === 'quick' ? toggleQuick() : toggleWidget(w.id)"
      >
        <WIcon :name="WIDGET_ICON[w.id] ?? 'cpu'" :size="20" />
      </button>
    </section>

    <!-- 右区段：时钟 / 鼠标 / 设置 -->
    <section class="seg tail-seg" data-tauri-drag-region>
      <span v-if="!compact" class="clock" data-tauri-drag-region>{{ clock }}</span>
      <button
        class="icon-btn"
        :class="{ on: liveOpen, lock: liveLocked }"
        :data-tip="liveTip"
        @click="toggleLive"
      >
        <WIcon :name="liveLocked ? 'lock' : 'live'" :size="20" />
      </button>
      <button class="icon-btn" :data-tip="t('bar.settings')" @click="openFlyout('settings')">
        <WIcon name="gear" :size="20" />
      </button>
      <button
        class="icon-btn fold-btn"
        :data-tip="t('bar.collapse')"
        @click="toggleFold"
      >
        <WIcon name="fold" :size="18" />
      </button>
    </section>

    <!-- 快捷指令抽屉 / 首次提示：都在独立的透明小窗 quick-drawer 里渲染，
         悬浮条窗口保持原尺寸，不会再露出毛玻璃黑底板 -->
  </div>
</template>

<style scoped>
.bar {
  position: relative;
  height: 68px;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 0 10px;
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  /* 8px = Win11 DWM 系统圆角半径，与窗口圆角化（glass.rs round_corners）对齐 */
  border-radius: 8px;
  box-shadow: var(--panel-inset), 0 6px 24px rgba(0, 0, 0, 0.45);
  width: max-content;
  /* 隐藏模式：整条向上翻出窗口（视觉上翻到屏幕上沿外）。
     位移必须伴随淡出：纯 transform 移出后该区域不再有新帧，
     WebView2 透明窗口会残留毛玻璃时的最后一帧（半透明鬼影） */
  transform-origin: top center;
  transition: transform 0.5s cubic-bezier(0.32, 1.25, 0.38, 1), opacity 0.4s ease;
  will-change: transform;
  /* 启动入场：从上方轻落 + 淡入（backwards 避免填充态锁死折叠位移） */
  animation: bar-in 0.5s cubic-bezier(0.25, 1.25, 0.4, 1) backwards;
}
@keyframes bar-in {
  from {
    opacity: 0;
    transform: translateY(-18px) scale(0.97);
  }
}
/* 手柄框选层描边：主题色包围整个主UI（焦点框高亮 + 光晕，非焦点框弱化）。
   outline 负偏移画在窗口内（窗口会被裁掉边界外内容），圆角略大于条体圆角 */
.bar.frame-focal {
  outline: 2.5px solid var(--accent, #4a72e8);
  outline-offset: -4px;
  box-shadow: var(--panel-inset), 0 6px 24px rgba(0, 0, 0, 0.45),
    0 0 16px var(--accent-strong, rgba(90, 140, 255, 0.55));
}
.bar.frame-dim {
  outline: 2px solid color-mix(in srgb, var(--accent, #4a72e8) 45%, transparent);
  outline-offset: -4px;
}
.bar.folded {
  transform: translateY(-108%);
  opacity: 0;
  /* opacity:0 的元素照样参与命中测试——缩放动画跑完前的 0.5s 里，
     这条看不见的条体仍会吃掉下面的点击，这里直接让它不接收鼠标 */
  pointer-events: none;
}
.bar.compact {
  height: 56px;
}
/* 折叠把手：悬浮条全部收起后挂在屏幕上沿的细条 */
.fold-handle {
  position: fixed;
  top: 0;
  left: 50%;
  transform: translateX(-50%) translateY(-120%);
  width: 46px;
  height: 13px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 0 0 8px 8px;
  background: rgba(40, 43, 48, 0.98);
  border: 1px solid rgba(255, 255, 255, 0.16);
  border-top: none;
  color: rgba(255, 255, 255, 0.6);
  opacity: 0;
  pointer-events: none;
  cursor: pointer;
  z-index: 5;
  transition: transform 0.32s cubic-bezier(0.3, 1.35, 0.4, 1) 0.16s,
    opacity 0.18s ease 0.16s, color 0.15s ease, background 0.15s ease,
    height 0.15s ease;
}
.fold-handle.show {
  transform: translateX(-50%) translateY(0);
  opacity: 1;
  pointer-events: auto;
}
.fold-handle:hover {
  color: #fff;
  background: rgba(54, 58, 64, 0.98);
  height: 16px;
}

.seg {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 4px;
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.05);
}
.bar.compact .seg {
  padding: 2px;
}

/* 左：游戏图标翻页（每页 5 个）+ 九宫格 */
.games-seg {
  gap: 6px;
}
.pager {
  position: relative;
  width: 264px; /* 5×46 + 4×6 + 两侧 5px 描边余量 */
  height: 58px;
  padding: 6px 5px;
  flex: none;
  /* 纵向放开裁剪：图标 hover 放大/启动脉冲不被切边；横向仍裁掉翻页轨道 */
  overflow-x: clip;
  overflow-y: visible;
  border-radius: 9px;
  cursor: grab;
  touch-action: pan-y;
}
.pager:active {
  cursor: grabbing;
}
.bar.compact .pager {
  width: 224px; /* 5×38 + 4×6 + 两侧 5px */
  height: 50px;
}
.track {
  display: flex;
  height: 100%;
  transition: transform 0.48s cubic-bezier(0.24, 1.22, 0.32, 1);
  will-change: transform;
}
.page {
  flex: none;
  width: 100%;
  padding: 0 5px; /* 描边余量：相邻页的图标不会漏进本页可视区 */
  display: flex;
  gap: 6px;
}
/* 非当前页整页隐藏（延迟到滑动动画结束才藏，保住翻页动画）：
   overflow-x: clip 在部分环境不裁剪合成层，第 6+ 个图标会漏到组件区；
   visibility 隐藏是确定性兜底，同时让手柄 checkVisibility 排除不可见图标 */
.page.off {
  visibility: hidden;
  transition: visibility 0s 0.5s;
  pointer-events: none;
}
.page .tile {
  width: 44px;
  height: 44px;
  flex: none;
}
.bar.compact .page .tile {
  width: 36px;
  height: 36px;
}
/* 页码圆点与翻页箭头已移除：滑动 / 滚轮翻页 */
.game-tile {
  position: relative;
  width: 46px;
  height: 46px;
  flex: none;
}
.bar.compact .game-tile {
  width: 38px;
  height: 38px;
}
.tile {
  position: relative;
  width: 100%;
  height: 100%;
  border-radius: 9px;
  /* 不能 overflow:hidden——启动光环 ::before 在图标外圈，裁了就看不见；
     圆角改由 img/.ph 自己承担 */
  cursor: pointer;
  background: rgba(255, 255, 255, 0.08);
  /* 弹簧曲线：hover 放大有「果冻」回弹手感 */
  transition: transform 0.24s cubic-bezier(0.34, 1.56, 0.64, 1);
  will-change: transform;
}
.tile:hover {
  transform: translateY(-2px) scale(1.14);
  z-index: 2;
  /* 悬停描边：跟随主题强调色（皮肤 accent）。
     用 inset 内描边——外侧描边会被翻页容器 overflow 裁掉，首尾图标只剩半圈 */
  box-shadow: 0 0 0 2px var(--accent, #ff7ac6), 0 0 14px var(--accent-strong, rgba(255, 122, 198, 0.55));
}
.tile:active {
  transform: scale(0.92);
  transition-duration: 0.09s;
}
/* 点击启动：弹起放大 + 高亮光环，再回弹归位 */
.tile.launching {
  animation: tile-launch 0.6s cubic-bezier(0.34, 1.56, 0.64, 1);
}
.tile.launching::before {
  content: "";
  position: absolute;
  inset: -3px;
  border-radius: 11px;
  border: 2px solid rgba(122, 162, 255, 0.8);
  animation: tile-ring 0.55s ease-out forwards;
  pointer-events: none;
}
/* 只动画 transform（可交给合成层）。原来在关键帧里动画 box-shadow 会每帧
   重绘整块发光区域（含 20px 模糊），是「点击图标时动画顿一下」的主要来源；
   光晕改由 .tile.launching::before 的 tile-ring 承担（只动 opacity/transform）。 */
@keyframes tile-launch {
  0% {
    transform: scale(1);
  }
  35% {
    transform: scale(1.2);
  }
  70% {
    transform: scale(0.94);
  }
  100% {
    transform: scale(1);
  }
}
@keyframes tile-ring {
  from {
    opacity: 0.9;
    transform: scale(0.92);
  }
  to {
    opacity: 0;
    transform: scale(1.35);
  }
}
.tile img {
  width: 100%;
  height: 100%;
  object-fit: cover;
  border-radius: 9px;
  pointer-events: none;
}
.tile .ph {
  display: flex;
  width: 100%;
  height: 100%;
  align-items: center;
  justify-content: center;
  font-size: 18px;
  font-weight: 600;
  color: #fff;
  border-radius: 9px;
  background: linear-gradient(135deg, #3a6df0, #7a3af0);
}
.tile.empty {
  display: flex;
  align-items: center;
  justify-content: center;
  color: rgba(255, 255, 255, 0.5);
  border: 1px dashed rgba(255, 255, 255, 0.28);
  background: transparent;
}

/* 图标按钮 */
.icon-btn {
  position: relative;
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 38px;
  height: 38px;
  border: 1px solid transparent;
  border-radius: 9px;
  background: transparent;
  color: #d6d8db;
  cursor: pointer;
  transition: background 0.12s ease, color 0.12s ease,
    transform 0.18s cubic-bezier(0.34, 1.56, 0.64, 1);
}
.bar.compact .icon-btn {
  width: 32px;
  height: 32px;
}
.icon-btn:hover {
  background: var(--accent-faint, rgba(255, 255, 255, 0.09));
  color: #fff;
  transform: translateY(-1px);
  box-shadow: 0 0 0 1.5px var(--accent, #ff7ac6), 0 0 10px var(--accent-strong, rgba(255, 122, 198, 0.4));
}
.icon-btn:active {
  background: rgba(255, 255, 255, 0.14);
  transform: scale(0.88);
  transition-duration: 0.08s;
}
.icon-btn.on {
  /* 组件打开态 = 主题色点亮（不再用白色），与左缘快捷开关一致 */
  background: var(--accent-faint);
  border-color: var(--accent-strong);
  color: var(--accent-hover);
  text-shadow: 0 0 8px var(--accent-strong);
  /* 开关组件时的点亮弹跳（类切换时触发一次） */
  animation: on-pop 0.28s cubic-bezier(0.34, 1.56, 0.64, 1);
}
@keyframes on-pop {
  0% {
    transform: scale(0.82);
  }
  60% {
    transform: scale(1.08);
  }
  100% {
    transform: scale(1);
  }
}
.icon-btn.on.lock {
  border-style: dashed;
  color: rgba(255, 255, 255, 0.75);
}
/* 快捷指令展开态：与左缘 HDR / A·B 同一套「点亮即主题色」规则，
   覆盖 .icon-btn.on 的白色高亮，保证与下方竖排图标同色 */
.icon-btn.quick-btn.on {
  color: var(--accent, #4a72e8);
  background: var(--accent-faint, rgba(90, 140, 255, 0.14));
  border-color: var(--accent-strong, rgba(90, 140, 255, 0.6));
}
.icon-btn.quick-btn.on:hover {
  color: var(--accent-hover, #5a80f0);
  background: var(--accent-mid, rgba(90, 140, 255, 0.32));
}

/* 快捷指令抽屉样式已迁到 src/quickdrawer/App.vue（独立透明小窗自身渲染） */

/* 首次启动提示卡样式同样在 src/quickdrawer/App.vue（tip 模式） */

/* 时钟 */
.clock {
  padding: 0 10px;
  font-size: 20px;
  font-weight: 600;
  letter-spacing: 0.5px;
  color: #f2f3f5;
  font-variant-numeric: tabular-nums;
}
.bar.compact .clock {
  display: none;
}

/* Tooltip 不在本窗口内渲染：独立小窗见 src/tooltip/App.vue（条内放气泡会
   遮游戏图标，窗口外又会被裁剪） */
</style>
