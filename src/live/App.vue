<script setup lang="ts">
// 直播画中画工具条：标签页 + 地址栏，管理 live-browser 浏览器窗口（顶层导航加载站点）。
// 浏览器窗口由 Rust 命令 live_browser_open 创建/导航（注入脚本修复 _blank 点击与窗口内全屏），
// 顶边可拖动直播画面、边/角 16:9 等比例缩放（live_chrome 原生镶边）。
// 锁定后浏览器鼠标点击穿透（不干扰游戏操作），工具条隐藏；Win+Shift+L 或悬浮条按钮解锁。
// 韧性：页面心跳断流（渲染进程挂掉/假死）→ 看门狗按当前标签自动重载；导航期间地址栏显示加载中。
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize, PhysicalPosition, PhysicalSize } from "@tauri-apps/api/dpi";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import WIcon from "../shared/WIcon.vue";
import { startGamepadNav } from "../shared/gamepad";
import { notifyReady } from "../shared/enter";
import { restoreFocus, wireClickFocus } from "../shared/api";
// 别名 tr：本文件内部用 t 表示 Tab 局部变量，避免遮蔽
import { t as tr } from "../shared/i18n";

const H_PICKER = 52; // 平台选择模式工具条逻辑高（含上下呼吸空间）
const H_TABS = 70; // 兜底逻辑高：实际高度以 .tb 实测为准（标签+收藏+地址三行）
const BAR_H = 68; // 与悬浮条 bar/App.vue 的 BAR_H 一致（初始打开位：悬浮条下方）
const LABEL = "live-browser";

interface Geo {
  x: number;
  y: number;
  w: number;
  h: number;
}
interface Tab {
  id: number;
  url: string;
  name: string;
}

const PLATFORMS = [
  { name: "B站", url: "https://www.bilibili.com", color: "#fb7299" },
  { name: "斗鱼", url: "https://www.douyu.com", color: "#ff5d23" },
  { name: "虎牙", url: "https://www.huya.com", color: "#f5a623" },
  { name: "抖音", url: "https://live.douyin.com", color: "#16e5d3" },
  { name: "YouTube", url: "https://www.youtube.com", color: "#ff2d2d" },
  { name: "Twitch", url: "https://www.twitch.tv", color: "#9146ff" },
];

const browserOpen = ref(false);
const locked = ref(false);
const fsMode = ref(false); // 视频全屏中（纯净模式：工具栏自动隐藏，退出后恢复）
// 收藏网址（多条）：地址栏星标增删当前页；选源屏集中管理（点击直达 / × 删除）
interface FavItem {
  url: string;
  name: string;
}
const favs = ref<FavItem[]>([]);
const activeTabUrl = computed(
  () => tabs.value.find((t) => t.id === activeId.value)?.url ?? ""
);
try {
  const saved = JSON.parse(localStorage.getItem("live.favs") ?? "[]");
  favs.value = Array.isArray(saved) ? saved.filter((f: FavItem) => f?.url) : [];
  const legacy = localStorage.getItem("live.fav"); // 旧单条收藏迁移
  if (legacy && !favs.value.some((f) => f.url === legacy)) {
    favs.value = [...favs.value, { url: legacy, name: shortName(legacy) }];
  }
  if (legacy) localStorage.removeItem("live.fav");
} catch {
  favs.value = [];
}
function saveFavs() {
  localStorage.setItem("live.favs", JSON.stringify(favs.value));
}
const isFav = computed(() => favs.value.some((f) => f.url === activeTabUrl.value));

function toggleFav() {
  const cur = activeTabUrl.value;
  if (!cur || cur === "about:blank") return;
  favs.value = isFav.value
    ? favs.value.filter((f) => f.url !== cur)
    : [...favs.value, { url: cur, name: shortName(cur) }];
  saveFavs();
}
/** 删除收藏（选源屏 × 按钮） */
function removeFav(url: string) {
  favs.value = favs.value.filter((f) => f.url !== url);
  saveFavs();
}
const loading = ref(false); // 页面导航加载中（地址栏进度条）
const addr = ref("");
const tabs = ref<Tab[]>([]);
const activeId = ref<number | null>(null);
let tabSeq = 1;

// 渲染进程看门狗：页面注入脚本每 3s 发一次心跳（live://hb），进程挂掉/假死后停摆；
// 超过 HB_GAP 收不到心跳且当前标签是真实页面，就按标签地址自动重载。
// 宽限定得比较松（>8 拍）：慢站点/后台节流不至于误判；连续 CRASH_RETRIES 次恢复
// 仍无心跳（如断网）就放弃，避免无限刷新循环。心跳一恢复计数即清零。
const HB_GAP = 25_000;
const CRASH_RETRIES = 3;
let lastHb = 0;
let crashRetries = 0;
let watchdogTimer: ReturnType<typeof setInterval> | null = null;

let content: WebviewWindow | null = null;
let unlistenContent: Array<() => void> = [];
let unlisten: Array<() => void> = [];
// 已知物理坐标（用于阻止 move 同步回环：自己 setPosition 的回声不再重设）
let tbPos: { x: number; y: number } | null = null;
let ctPos: { x: number; y: number } | null = null;
let tbW = 0;
let ctW = 0;
let ctH = 0;
let tbH = H_PICKER;
// 工具条初次打开时的位置：关闭浏览器后回到这里，而不是停在旧视频上方
let homeGeo: { x: number; y: number } | null = null;
// 最近一次程序化移动浏览器内容的时刻：静默期内的 move 事件一律视为自身回声
let lastCtSyncAt = 0;

function broadcast() {
  void emit("live://state", {
    browser: browserOpen.value,
    locked: locked.value,
  });
}

/* ---------------- 标签页 ---------------- */

function shortName(u: string): string {
  try {
    const u2 = new URL(u);
    const host = u2.host.replace(/^www\./, "");
    const seg = u2.pathname.split("/").filter(Boolean).pop();
    const s = seg ? `${host}/${seg}` : host;
    return s.length > 22 ? s.slice(0, 21) + "…" : s;
  } catch {
    return u.slice(0, 22);
  }
}

function normalizeUrl(raw: string): string | null {
  const s = raw.trim();
  if (!s) return null;
  return /^https?:\/\//i.test(s) ? s : `https://${s}`;
}

/** 在标签页中打开地址：同地址复用既有标签；浏览器窗口不存在时创建 */
async function openTab(rawUrl: string) {
  const url = normalizeUrl(rawUrl);
  if (!url) return;
  let t = tabs.value.find((x) => x.url === url);
  if (!t) {
    t = { id: tabSeq++, url, name: shortName(url) };
    tabs.value.push(t);
  }
  activeId.value = t.id;
  addr.value = url;
  loading.value = true; // 乐观置位，live://load 事件接管开始/结束
  await ensureBrowser(url);
}

async function activateTab(id: number) {
  if (activeId.value === id) return;
  const t = tabs.value.find((x) => x.id === id);
  if (!t) return;
  activeId.value = id;
  addr.value = t.url;
  if (t.url) {
    loading.value = true;
    await invoke("live_browser_open", { url: t.url }).catch((e) =>
      console.error("切换标签失败", e)
    );
  }
}

async function closeTab(id: number) {
  const i = tabs.value.findIndex((x) => x.id === id);
  if (i < 0) return;
  tabs.value.splice(i, 1);
  if (activeId.value !== id) return;
  const next = tabs.value[i] ?? tabs.value[i - 1];
  if (next) {
    await activateTab(next.id);
  } else {
    await goHome();
  }
}

async function newTab() {
  const t: Tab = { id: tabSeq++, url: "", name: tr("live.newTab") };
  tabs.value.push(t);
  activeId.value = t.id;
  addr.value = "";
  // 空白页占位，地址栏输入后就地导航
  await invoke("live_browser_open", { url: "about:blank" }).catch(() => {});
}

const goHome = async () => {
  tabs.value = [];
  activeId.value = null;
  const tb = getCurrentWindow();
  await destroyBrowser(true);
  // 回到工具条最初的位置（平台选择状态），而不是停在旧视频上沿
  if (homeGeo) {
    const scale = await tb.scaleFactor();
    await tb.setSize(new LogicalSize(660, H_PICKER));
    await tb.setPosition(new PhysicalPosition(homeGeo.x, homeGeo.y));
    tbPos = { x: homeGeo.x, y: homeGeo.y };
    tbW = Math.round(660 * scale);
    tbH = Math.round(H_PICKER * scale);
    // 回选源屏后内容宽已变：左缘钉在 homeGeo 上做宽度自适应
    // （await + fitBusy：成为最终态，不被并发 RO 适配拽偏）
    await fitPickerWidth({ anchorLeft: true });
    return;
  }
  const x = tbPos?.x ?? 0;
  const cy = (tbPos?.y ?? 0) + tbH; // 原工具条底沿作为内容参照，收回单行
  await placeToolbarAbove(x, cy, tbW || 660);
};
const go = () => {
  const u = normalizeUrl(addr.value);
  if (u) void openTab(u);
};
const goPlatform = (p: { url: string }) => void openTab(p.url);

/* ---------------- 选源屏宽度自适应 ----------------
 * 选源行是「手柄 + 提示 + 6 平台 + 收藏 × N」的整行布局：窗口固定宽时按钮
 * 会被压到极窄，文字溢出胶囊互相叠印（收藏是 22 字符的长链接名，重灾区）。
 * 与悬浮条同思路：内容多宽窗口就多宽（max-content 实测），夹在显示器内并
 * 保持左右居中；浏览器打开后仍由 placeToolbarAbove 按内容窗宽对齐，不参与。
 * ResizeObserver 兜底文案/收藏增减后的宽度变化（含 i18n 切换、字体晚就绪）。 */

let fitTimer: ReturnType<typeof setTimeout> | null = null;
let fitBusy = false; // goHome 锚定适配进行中：吞掉并发的 RO 自适应，防止左缘被中途拽偏
function scheduleFit() {
  // 浏览器态：.tb 行数变化（收藏行增减）会改工具条实测高度，重新贴合内容窗
  if (browserOpen.value) {
    if (ctPos) void placeToolbarAbove(ctPos.x, ctPos.y, ctW);
    return;
  }
  if (fitTimer) clearTimeout(fitTimer);
  fitTimer = setTimeout(() => void fitPickerWidth(), 60);
}

async function fitPickerWidth(opts: { resetY?: boolean; anchorLeft?: boolean } = {}) {
  if (browserOpen.value || fitBusy) return;
  fitBusy = true;
  try {
    await fitPickerWidthInner(opts);
  } finally {
    fitBusy = false;
  }
}

async function fitPickerWidthInner(opts: { resetY?: boolean; anchorLeft?: boolean }) {
  if (browserOpen.value) return;
  await nextTick();
  const el = document.querySelector(".tb") as HTMLElement | null;
  if (!el) return;
  const win = getCurrentWindow();
  const scale = await win.scaleFactor();
  const mon = await currentMonitor();
  // offsetWidth 是布局宽，不受窗口当前宽度限制（.tb.single 用 max-content 撑开）
  const wL = Math.min(el.offsetWidth, Math.max(320, Math.floor((mon?.size.width ?? 1920) / scale) - 24));
  const w = Math.round(Math.max(320, wL) * scale);
  const h = Math.round(H_PICKER * scale);
  const pos = await win.outerPosition();
  let x: number;
  let y: number;
  if (opts.resetY) {
    // 首次创建：所在显示器顶部居中、悬浮条下方
    x = Math.round((mon?.position.x ?? 0) + ((mon?.size.width ?? w) - w) / 2);
    y = Math.round((mon?.position.y ?? 0) + (BAR_H + 12) * scale);
  } else if (opts.anchorLeft) {
    // goHome 回原位：左缘钉在已恢复的 homeGeo.x 上，宽度只向右伸缩
    x = pos.x;
    y = pos.y;
  } else {
    // 收藏增减/文案变化：围绕当前窗口中心伸缩，不左右跳、不强行回屏幕正中
    const curW = (await win.outerSize()).width;
    x = pos.x + Math.round((curW - w) / 2);
    y = pos.y;
  }
  // 与 goHome / placeToolbarAbove 共用一套几何记录，避免回选源屏后尺寸错乱
  tbPos = { x, y };
  tbW = w;
  tbH = h;
  await win.setSize(new PhysicalSize(w, h));
  await win.setPosition(new PhysicalPosition(x, y));
}

/* ---------------- 工具条 / 浏览器窗口位置同步 ---------------- */

async function placeToolbarAbove(contentX: number, contentY: number, contentW: number) {
  const scale = await getCurrentWindow().scaleFactor();
  // 实测 DOM 高度：浏览器态是「标签 + 收藏 + 地址」三行，老的 H_TABS=70 双行常数
  // 会把窗口压矮导致行被裁（用户截图：顶部导航栏字体被截一半）
  const el = document.querySelector(".tb") as HTMLElement | null;
  // .tb 是 height:100vh，offsetHeight 恒等于窗高（自锁）——必须用 scrollHeight
  // 取内容真实高度，窗口才能长到三行的实际高度
  const h = Math.round((el?.scrollHeight || H_TABS) * scale);
  const mon = await currentMonitor();
  const minY = (mon?.position.y ?? 0) + 2;
  let y = contentY - h;
  let cx = contentX;
  let cy = contentY;
  // 拖到屏幕顶时工具条整体出屏被裁：钳住工具条贴屏内，同时把内容下推，
  // 两块始终严丝合缝且完整可见
  if (y < minY) {
    y = minY;
    cy = y + h;
  }
  if (tbPos && tbPos.x === cx && tbPos.y === y && tbW === contentW && tbH === h) return;
  tbPos = { x: cx, y };
  syncCt = { x: cx, y: cy };
  tbW = contentW;
  tbH = h;
  // 节流：事件风暴（拖拽回声、贴靠、页面跳转连发）时限制真实 setPosition 频率，
  // 防止宿主被窗口定位调用钉满 CPU、工具条输入被饿死（按键按不动）。
  // 拖尾定时器保证最后一次位置必达；风暴发生时打日志便于确诊。
  const now = Date.now();
  if (tbSyncTimer) {
    if (!tbSyncThrottleLogged) {
      tbSyncThrottleLogged = true;
      console.error("[live] 几何同步节流触发：窗口位置事件风暴");
    }
    return;
  }
  const wait = Math.max(0, 80 - (now - tbSyncAt));
  tbSyncTimer = setTimeout(() => {
    tbSyncTimer = null;
    tbSyncAt = Date.now();
    if (!tbPos) return;
    const tb = getCurrentWindow();
    tb.setPosition(new PhysicalPosition(tbPos.x, tbPos.y)).catch(() => {});
    tb.setSize(new PhysicalSize(tbW, tbH)).catch(() => {});
    if (syncCt) void placeContent(syncCt.x, syncCt.y);
  }, wait);
}
let tbSyncTimer: ReturnType<typeof setTimeout> | null = null;
let syncCt: { x: number; y: number } | null = null; // 本次同步应把内容窗推到的位置（钳制时≠入参）
let tbSyncAt = 0;
let tbSyncThrottleLogged = false;
let fitRo: ResizeObserver | null = null;

async function placeContent(x: number, y: number) {
  if (!content) return;
  if (ctPos && ctPos.x === x && ctPos.y === y) return;
  ctPos = { x, y };
  lastCtSyncAt = Date.now();
  content.setPosition(new PhysicalPosition(x, y)).catch(() => {});
}

function onToolbarMoved(p: { x: number; y: number }) {
  tbPos = p;
  // 拖工具条 = 移动浏览器。不 await：handler 同步跑完，事件严格按序生效，
  // 不会因交错把滞后的旧 setPosition 排到新位置后面（那会让窗口来回拽跳）。
  void placeContent(p.x, p.y + tbH);
}

/**
 * 浏览器窗口无装饰、无拖拽区（拖动走原生镶边），onMoved 基本是自身 setPosition
 * 的回声：只记录坐标，绝不回推工具条——回推会形成互抖循环（窗口跳动）。
 * 仅当移动明显来自外部（原生拖动/Win+方向键贴靠）且距上次程序化同步超过静默期时才对齐。
 */
function onContentMoved(p: { x: number; y: number }) {
  const isEcho = ctPos !== null && ctPos.x === p.x && ctPos.y === p.y;
  ctPos = p;
  if (!isEcho && Date.now() - lastCtSyncAt > 400 && ctW > 0) {
    void placeToolbarAbove(p.x, p.y, ctW);
  }
  saveGeoDebounced();
}

async function onContentResized(s: { width: number; height: number }) {
  ctW = s.width;
  ctH = s.height;
  // 拖左/上边缘缩放会改窗口左上角坐标：读实时位置把工具条贴齐，
  // 随后的 move 事件自然被 ctPos 判为回声，不再二次触发。
  if (content) {
    try {
      const p = await content.outerPosition();
      ctPos = { x: p.x, y: p.y };
      lastCtSyncAt = Date.now();
      await placeToolbarAbove(ctPos.x, ctPos.y, ctW);
    } catch {
      /* 窗口已销毁 */
    }
  }
  saveGeoDebounced();
}

/* ---------------- 几何持久化（物理像素） ---------------- */

let geoTimer: ReturnType<typeof setTimeout> | null = null;
function saveGeoDebounced() {
  if (geoTimer) clearTimeout(geoTimer);
  geoTimer = setTimeout(() => {
    if (ctPos && ctW && ctH) {
      localStorage.setItem(
        "live.geo",
        JSON.stringify({ x: ctPos.x, y: ctPos.y, w: ctW, h: ctH })
      );
    }
  }, 400);
}

function readGeo(): Geo | null {
  try {
    const g = JSON.parse(localStorage.getItem("live.geo") ?? "null") as Geo | null;
    if (g && [g.x, g.y, g.w, g.h].every(Number.isFinite)) return g;
  } catch {
    /* 忽略损坏数据 */
  }
  return null;
}

/** 首次打开：所在显示器右下角 960×540（逻辑 px），避开任务栏 */
async function defaultGeo(): Promise<Geo> {
  const mon = await currentMonitor();
  const sc = mon?.scaleFactor ?? 1;
  const monWL = (mon?.size.width ?? 1920) / sc;
  const monHL = (mon?.size.height ?? 1080) / sc;
  const wL = Math.min(960, Math.round(monWL * 0.5));
  const hL = Math.round((wL * 9) / 16);
  const bx = mon?.position.x ?? 0;
  const by = mon?.position.y ?? 0;
  return {
    x: Math.round(bx + (monWL - wL - 24) * sc),
    y: Math.round(by + (monHL - hL - 60) * sc),
    w: Math.round(wL * sc),
    h: Math.round(hL * sc),
  };
}

/* ---------------- 浏览器窗口生命周期 ---------------- */

/** 等待旧窗口彻底销毁，避免同 label 重建冲突 */
async function waitLabelFree() {
  for (let i = 0; i < 40; i++) {
    if (!(await WebviewWindow.getByLabel(LABEL))) return;
    await new Promise((r) => setTimeout(r, 50));
  }
}

async function attachContent(win: WebviewWindow) {
  content = win;
  unlistenContent.push(
    await win.onMoved((e) => void onContentMoved(e.payload)),
    await win.onResized((e) => void onContentResized(e.payload))
  );
  win.once("tauri://destroyed", () => {
    if (content !== win) return;
    content = null;
    ctPos = null;
    unlistenContent.forEach((f) => f());
    unlistenContent = [];
    browserOpen.value = false;
    locked.value = false;
    loading.value = false;
    lastHb = 0;
    broadcast();
  });
}

async function destroyBrowser(broadcastChange: boolean) {
  unlistenContent.forEach((f) => f());
  unlistenContent = [];
  // 窗口没了，「无指针」状态要一起归零：下次新建窗口仍会按 Rust 侧的静态标记补施
  void invoke("live_cursor_hidden", { on: false });
  // 手柄控制/锁定模式一并复位（后端 tick 也会自动降级，这里显式复位让按钮态即时正确）
  void invoke("livepad_set_mode", { m: 0 }).catch(() => {});
  const w = content;
  content = null;
  ctPos = null;
  browserOpen.value = false;
  locked.value = false;
  loading.value = false;
  lastHb = 0;
  crashRetries = 0;
  if (w) {
    try {
      await w.close();
    } catch {
      /* 可能已销毁 */
    }
  }
  if (broadcastChange) broadcast();
}

/** 从 Rust 侧同步锁定态：页面重载/看门狗恢复后 locked 镜像可能与窗口实际样式
 *  （点击穿透 + cursor:none 还挂着）脱节——以 Rust 的 CURSOR_HIDDEN 为准，
 *  否则锁定会被悄悄「复位」，表现为锁住看视频失效 */
async function syncLockFromRust() {
  locked.value = await invoke<boolean>("live_lock_state").catch(() => false);
}

/** 确保浏览器窗口存在并导航到 url（已存在则仅导航；几何与显示由本工具条负责） */
async function ensureBrowser(url: string) {
  const existing = await WebviewWindow.getByLabel(LABEL);
  if (!existing) {
    // 首次开浏览器前记录工具条原位（关闭浏览器后回到这里）
    if (!homeGeo) {
      const p = await getCurrentWindow().outerPosition();
      homeGeo = { x: p.x, y: p.y };
    }
    await destroyBrowser(false);
    await waitLabelFree();
    const geo = readGeo() ?? (await defaultGeo());
    await invoke("live_browser_open", { url }).catch((e) => {
      console.error("创建直播窗口失败", e);
      throw e;
    });
    let win: WebviewWindow | null = null;
    for (let i = 0; i < 60 && !win; i++) {
      win = await WebviewWindow.getByLabel(LABEL);
      if (!win) await new Promise((r) => setTimeout(r, 50));
    }
    if (!win) {
      broadcast();
      return;
    }
    // 远端页面加载前先给深色底，避免白屏闪烁
    await win.setBackgroundColor("#141518").catch(() => {});
    await win.setPosition(new PhysicalPosition(geo.x, geo.y));
    await win.setSize(new PhysicalSize(geo.w, geo.h));
    await win.show();
    ctPos = { x: geo.x, y: geo.y };
    ctW = geo.w;
    ctH = geo.h;
    await attachContent(win);
  }
  // 原生窗口镶边：顶边可拖动直播画面，边/角 16:9 等比例缩放
  invoke("attach_live_chrome").catch(() => {});
  browserOpen.value = true;
  // 锁定态以 Rust 侧为准（新建的窗口天然未锁定，行为不变）；真锁定中被恢复链
  // 走到这里时保持锁定，指针隐藏也不撤销——不能把用户的锁悄悄解掉
  await syncLockFromRust();
  if (!locked.value) void invoke("live_cursor_hidden", { on: false });
  // 导航/建窗刚发生，重置心跳基线：给慢站点留出加载宽限，避免看门狗误判
  lastHb = Date.now();
  await nextTick(); // 等 .tb 切到三行布局，实测高度才有意义
  if (ctPos) await placeToolbarAbove(ctPos.x, ctPos.y, ctW);
  broadcast();
}

/**
 * 看门狗：心跳断流超过 HB_GAP → 判定渲染进程挂掉/页面假死，按当前标签地址重载。
 * 复用 live_browser_open（窗口还在则 navigate，渲染进程随之重建、注入脚本重新生效）；
 * 窗口本身没了则走 ensureBrowser 全量重建。about:blank 空标签无从恢复，跳过。
 */
async function watchdogTick() {
  if (!browserOpen.value || Date.now() - lastHb < HB_GAP) return;
  const url = tabs.value.find((x) => x.id === activeId.value)?.url;
  if (!url || url.startsWith("about:")) return;
  if (crashRetries >= CRASH_RETRIES) {
    console.warn("直播页面反复崩溃，已停止自动重载，请手动刷新或换站");
    return;
  }
  crashRetries++;
  lastHb = Date.now(); // 恢复期间不再重复触发
  console.warn("直播页面心跳断流，自动重载：", url);
  loading.value = true;
  if (await WebviewWindow.getByLabel(LABEL)) {
    await invoke("live_browser_open", { url }).catch(() => ensureBrowser(url));
  } else {
    await ensureBrowser(url);
  }
}

/* ---------------- 锁定（点击穿透）/ 全屏纯净模式 / 关闭 ---------------- */

/** 视频进入/退出全屏时隐藏/恢复工具栏（纯净观看）。锁定状态下退出全屏不恢复显示 */
async function applyFs(v: boolean) {
  if (v === fsMode.value) return;
  fsMode.value = v;
  const tb = getCurrentWindow();
  try {
    if (v) {
      await tb.hide();
    } else if (!locked.value && browserOpen.value) {
      await tb.show();
    }
  } catch (e) {
    console.error("全屏切换工具栏显隐失败", e);
  }
}

/** 按住手柄拖动整个直播窗口（工具条 + 画面一起移动，位置自动记忆） */
function startMove() {
  getCurrentWindow().startDragging();
}

/** 直播页面历史导航（前进 / 后退） */
const navHistory = (dir: "back" | "forward") => {
  invoke("live_browser_nav", { dir }).catch((e) => console.error("导航失败", e));
};

async function applyLock(v: boolean) {
  // 先用 Rust 的真实模式（padMode，模式 2 = 锁定）纠正本地 locked：
  // B 返回链、GameBar 按钮、热键等路径都会直接改 Rust 侧模式，而前端不知道，
  // 变旧之后「切换锁定」就会往反方向走（该锁反而解锁）=「按 X 没反应/UI 不隐藏」。
  const realLocked = padMode.value === 2;
  if (realLocked !== locked.value) locked.value = realLocked;
  if (v === locked.value || !content || !browserOpen.value) return;
  locked.value = v;
  try {
    if (v) {
      // 只设点击穿透，不加 LAYERED（否则全屏游戏独立翻转时画面会消失只剩声音）
      await invoke("set_click_through", { label: "live-browser", on: true });
      // ① 先把系统前台还给游戏：游戏拿到前台才会隐藏系统光标（多数游戏在前台时
      //    ShowCursor(FALSE) 或交回自己的光标）。锁定后箭头仍浮在画面上，根因
      //    就是焦点停在我们的窗口上（点锁定按钮时工具条刚抢过焦点）。
      //    顺序不能反：必须「先还前台、后 hide」，否则 Windows 会把前台塞给
      //    Z 序里的下一个窗口（跳桌面 / 任务视图）。
      //    必须 await：不等它完成就 hide，我们一 hide 前台就落到别人手上，
      //    Rust 侧再判断「前台是不是我们」直接早退，游戏根本拿不回前台。
      await restoreFocus().catch(() => {});
      // ② 指针停在视频区域上时，光标形状由我们这个窗口决定（穿透只管命中测试），
      //    系统箭头照样露出来 —— 压一层 cursor:none，等价于「把鼠标移到视频外侧」。
      await invoke("live_cursor_hidden", { on: true });
      getCurrentWindow().hide();
    } else {
      await invoke("live_cursor_hidden", { on: false });
      await invoke("set_click_through", { label: "live-browser", on: false });
      // 全屏纯净模式中解锁：不弹回工具栏，避免遮挡画面
      if (!fsMode.value) await getCurrentWindow().show();
      // 被全屏游戏等 topmost 窗口盖住后重新置顶，确保解锁即可见
      await content.setAlwaysOnTop(true);
    }
  } catch (e) {
    console.error("锁定切换失败", e);
  }
  // 与手柄控制联动：锁定 → 模式 2（游戏持焦，仅 B 长按语音被消费）；
  // 解锁 → 模式 0（手柄交还，浏览器重获焦点可继续网页交互）。
  // 不 Reload、不重建 WebView2——只切逻辑输入路由 + Windows 焦点。
  void invoke("livepad_set_mode", { m: v ? 2 : 0 }).catch(() => {});
  if (!v) void content?.setFocus().catch(() => {});
  broadcast();
}

/* ---------------- 手柄控制（ControllerInputRouter 前端接线） ----------------
 * 后端 live_pad.rs 是路由与虚拟鼠标本体；这里只做三件事：
 * 1) 「🎮」按钮切换控制态（livepad_set_mode），失败把后端中文原因 toast 出来；
 * 2) 响应后端移窗/缩放指令（LT+摇杆 / LT / RT）——几何操作全部复用既有的
 *    placeToolbarAbove/placeContent/setSize 通道，不另起一套坐标同步；
 * 3) 与「锁定」联动：锁定 → 模式 2（游戏焦点 + B 长按语音），解锁 → 回 0。
 * 语音 UI 不在本窗口渲染（锁定时工具条隐藏），由独立 voice-hud 窗口负责。 */
const padMode = ref(0);
const toast = ref("");
let toastTimer: ReturnType<typeof setTimeout> | null = null;
function showToast(msg: string) {
  toast.value = msg;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toast.value = ""), 3000);
}

async function togglePad() {
  const m = padMode.value === 1 ? 0 : 1;
  try {
    await invoke("livepad_set_mode", { m });
    showToast(m === 1 ? tr("live.padOn") : tr("live.padOff"));
  } catch (e) {
    // 后端拒绝（注入未就绪等）：显示中文原因，不进入控制态
    showToast(String(e));
  }
}

/** 16:9 等比例缩放直播窗口（物理像素夹取：最小 320×180，最大显示器 ~92%）。
 *  只改内容窗尺寸；工具条贴合由 onResized 回声统一处理 */
async function padZoom(dir: number, step: number) {
  if (!content || ctW <= 0) return;
  const mon = await currentMonitor();
  const scale = await getCurrentWindow().scaleFactor();
  const minW = 320 * scale;
  const maxW = Math.max(minW + 1, (mon?.size.width ?? 1920) * 0.92);
  const maxH = Math.max(180 * scale + 1, (mon?.size.height ?? 1080) * 0.92);
  let w = ctW * (1 + dir * step);
  w = Math.min(Math.max(w, minW), Math.min(maxW, maxH * (16 / 9)));
  const h = (w * 9) / 16;
  content.setSize(new PhysicalSize(Math.round(w), Math.round(h))).catch(() => {});
}

/** LT+左摇杆：增量移动整个直播（工具条跟着贴）。placeContent 立即记坐标，
 *  保证 16ms 连发时累加基准是最新目标位而非 80ms 前的旧位 */
function padMoveBy(dx: number, dy: number) {
  if (!content || !ctPos) return;
  const nx = Math.round(ctPos.x + dx);
  const ny = Math.round(ctPos.y + dy);
  void placeContent(nx, ny);
  void placeToolbarAbove(nx, ny, ctW);
}

function padMoveTo(x: number, y: number) {
  if (!content) return;
  void placeContent(x, y);
  void placeToolbarAbove(x, y, ctW);
}

async function closeAll() {
  tabs.value = [];
  activeId.value = null;
  locked.value = false;
  void invoke("live_cursor_hidden", { on: false });
  browserOpen.value = false;
  broadcast();
  await destroyBrowser(false);
  await getCurrentWindow().close();
}

/* ---------------- 生命周期 ---------------- */

onMounted(async () => {
  const tbEl = document.querySelector(".tb");
  if (tbEl && typeof ResizeObserver !== "undefined") {
    fitRo = new ResizeObserver(() => scheduleFit());
    fitRo.observe(tbEl);
  }
  // 首帧前先按内容自定位（宽随内容 + 悬浮条下方水平居中）：窗口此刻仍隐藏，
  // 定位完成才 notifyReady → showWhenReady 显示，不闪位置。定位必须本页自己做：
  // 悬浮条的 created 回调按 480 初始宽居中且完成较晚，会把这里已按内容宽
  // 居中的窗口再拽偏（整条右移伸出屏幕被裁）。
  if (!(await WebviewWindow.getByLabel(LABEL))) {
    await fitPickerWidth({ resetY: true });
  }
  notifyReady(); // 首帧就绪 → 悬浮条才 show 工具条（避免白框）
  // 覆盖模式：游戏里点击工具条才取系统焦点，地址栏输入等键盘操作才进得来
  wireClickFocus(getCurrentWindow().label);
  // 浏览器窗口已存在（如前端热重载）→ 收编并对齐工具条
  const existing = await WebviewWindow.getByLabel(LABEL);
  if (existing) {
    try {
      await existing.setIgnoreCursorEvents(false);
    } catch {
      /* 忽略 */
    }
    const p = await existing.outerPosition();
    const s = await existing.outerSize();
    ctPos = { x: p.x, y: p.y };
    ctW = s.width;
    ctH = s.height;
    await attachContent(existing);
    const t: Tab = { id: tabSeq++, url: addr.value || "about:blank", name: tr("live.page") };
    browserOpen.value = true;
    tabs.value = [t];
    activeId.value = t.id;
    // 热重载收编旧窗口：锁定态跟着 Rust 走（窗口可能还处于点击穿透中）
    await syncLockFromRust();
    await placeToolbarAbove(ctPos.x, ctPos.y, ctW);
  }
  addr.value = localStorage.getItem("live.url") ?? "";
  // 打开看直播先停在选源屏，由用户自己选平台/收藏（不自动跳转上次地址）

  const tb = getCurrentWindow();
  unlisten.push(
    await tb.onMoved((e) => void onToolbarMoved(e.payload)),
    // 页面内导航（点链接/跳转）→ 更新当前标签地址与名称
    await listen<string>("live://nav", (e) => {
      const url = String(e.payload ?? "");
      if (!url || url.startsWith("about:")) return;
      const t = tabs.value.find((x) => x.id === activeId.value);
      if (t) {
        t.url = url;
        t.name = shortName(url);
      }
      addr.value = url;
    }),
    // 页面加载开始/结束 → 地址栏加载中状态
    await listen<{ url: string; phase: "start" | "end" }>("live://load", (e) => {
      loading.value = e.payload?.phase === "start";
      // 导航在推进 = 渲染进程活着，顺带续期心跳基线（慢站点不至于被误判）
      lastHb = Date.now();
    }),
    // 注入脚本心跳：收到即视为存活，并清零崩溃重试计数
    await listen<number>("live://hb", () => {
      lastHb = Date.now();
      crashRetries = 0;
    }),
    await listen<boolean>("live://set-locked", (e) =>
      void applyLock(e.payload === true)
    ),
    // 直播页面注入脚本回报的全屏状态 → 纯净模式显隐工具栏
    await listen<boolean>("live://fs", (e) => void applyFs(e.payload === true)),
    // 右下角等比例缩放手柄回报的目标尺寸（页面 CSS 像素，16:9）。
    // 注意：ctW/ctH 是物理像素，这里不改，等 onResized 回声统一更新，避免逻辑/物理混用错位
    await listen<{ w: number; h: number }>("live://resize", (e) => {
      const p = e.payload;
      if (content && p && Number.isFinite(p.w) && Number.isFinite(p.h)) {
        const w = Math.max(320, Math.round(p.w));
        const h = Math.max(180, Math.round(p.h));
        content.setSize(new LogicalSize(w, h)).catch(() => {});
      }
    }),
    await listen("live://toggle-lock", () => {
      if (!browserOpen.value) return;
      // 目标状态以 Rust 模式为准（padMode 由 livepad://mode 实时同步），
      // 不看本地 locked —— 它可能已经被别的路径改旧了。
      void applyLock(padMode.value !== 2);
    }),
    await listen("live://focus", async () => {
      await tb.show();
      await tb.setFocus();
    }),
    // 攻略助手（Rust guide.rs）：请求在浏览器中打开地址 → 走标准 openTab 链路
    await listen<string>("live://open-url", (e) => {
      const u = String(e.payload ?? "");
      if (u) void openTab(u);
    }),
    // ---- 手柄控制后端指令（live_pad.rs → 本窗口） ----
    await listen<number>("livepad://mode", (e) => {
      padMode.value = Number(e.payload) || 0;
      // 模式是锁定状态的唯一真相：本地 locked 落后了就地纠偏，否则下次切换会反向
      const realLocked = padMode.value === 2;
      if (realLocked !== locked.value) locked.value = realLocked;
      // 全屏纯净模式里进控制态也不弹工具条（锁定/返回靠 X / B，不用工具条），
      // 否则全屏画面上会突然多出一条导航栏。
      if (fsMode.value && padMode.value === 1) {
        void getCurrentWindow().hide();
      }
    }),
    await listen<{ dx: number; dy: number }>("livepad://move", (e) => {
      const p = e.payload;
      if (p && Number.isFinite(p.dx) && Number.isFinite(p.dy)) padMoveBy(p.dx, p.dy);
    }),
    await listen<{ x: number; y: number }>("livepad://move-to", (e) => {
      const p = e.payload;
      if (p && Number.isFinite(p.x) && Number.isFinite(p.y)) padMoveTo(p.x, p.y);
    }),
    await listen<{ dir: number; step: number }>("livepad://zoom", (e) => {
      const p = e.payload;
      if (p && Number.isFinite(p.dir) && Number.isFinite(p.step))
        void padZoom(p.dir > 0 ? 1 : -1, Math.abs(p.step));
    })
  );
  // 初始模式镜像（热重载/重开工具条时按钮态不丢）
  void invoke<{ mode: number }>("livepad_status")
    .then((s) => (padMode.value = s?.mode ?? 0))
    .catch(() => {});
  watchdogTimer = setInterval(() => void watchdogTick(), 5_000);
  // 手柄导航：B = 返回（浏览器打开中 → 收回浏览器；平台选择态 → 关闭工具条）
  unlisten.push(
    startGamepadNav({
      onBack: () => {
        if (browserOpen.value) void goHome();
        else void closeAll();
      },
    })
  );
  broadcast();
});

onBeforeUnmount(() => {
  if (watchdogTimer) clearInterval(watchdogTimer);
  fitRo?.disconnect();
  if (fitTimer) clearTimeout(fitTimer);
  unlisten.forEach((f) => f());
  unlistenContent.forEach((f) => f());
});
</script>

<template>
  <div class="tb" :class="{ single: !browserOpen }" data-tauri-drag-region>
    <!-- 平台快捷入口（浏览器未打开时） -->
    <template v-if="!browserOpen">
      <button
        class="op grip"
        :title="tr('live.dragWindow')"
        @pointerdown.stop.prevent="startMove"
      >
        <WIcon name="hand" :size="15" />
      </button>
      <span class="hint" data-tauri-drag-region>{{ tr("live.watch") }}</span>
      <button
        v-for="p in PLATFORMS"
        :key="p.url"
        class="plat"
        :title="tr('live.platformLive', { name: p.name })"
        @click="goPlatform(p)"
      >
        <i class="dot" :style="{ background: p.color }" /><span class="pname">{{ p.name }}</span>
      </button>
      <!-- 收藏网址管理：点击直达，× 删除 -->
      <template v-if="favs.length">
        <span class="vsep" data-tauri-drag-region></span>
        <button
          v-for="f in favs"
          :key="f.url"
          class="plat fav"
          :title="tr('live.openFav') + f.name"
          @click="openTab(f.url)"
        >
          <i class="dot fav-dot"><WIcon name="starFill" :size="11" /></i><span class="pname">{{ f.name }}</span>
          <span class="favx" :title="tr('live.clearFav')" @click.stop="removeFav(f.url)">
            <WIcon name="close" :size="9" />
          </span>
        </button>
      </template>
      <button class="op danger" :title="tr('live.closeAll')" @click="closeAll">
        <WIcon name="close" :size="15" />
      </button>
    </template>

    <!-- 标签页行 + 地址栏行（浏览器已打开时） -->
    <template v-else>
      <div class="row" data-tauri-drag-region>
        <button
          class="op grip"
          :title="tr('live.dragWindow')"
          @pointerdown.stop.prevent="startMove"
        >
          <WIcon name="hand" :size="15" />
        </button>
        <button class="op" :title="tr('live.back')" @click="navHistory('back')">
          <WIcon name="chevL" :size="15" />
        </button>
        <button class="op" :title="tr('live.forward')" @click="navHistory('forward')">
          <WIcon name="chevR" :size="15" />
        </button>
        <button class="op" :title="tr('live.closeBrowser')" @click="goHome">
          <WIcon name="home" :size="15" />
        </button>
        <div class="tabstrip">
          <div
            v-for="t in tabs"
            :key="t.id"
            class="tab"
            data-nav
            :class="{ on: t.id === activeId }"
            :title="t.url"
            @click="activateTab(t.id)"
            @auxclick.middle="closeTab(t.id)"
          >
            <span class="tname">{{ t.name }}</span>
            <span class="tclose" :title="tr('live.closeTab')" @click.stop="closeTab(t.id)">
              <WIcon name="close" :size="9" />
            </span>
          </div>
          <button class="op slim" :title="tr('live.newTab')" @click="newTab">
            <WIcon name="plus" :size="13" />
          </button>
        </div>
      </div>
      <!-- 收藏栏：点击直达，当前页高亮 -->
      <div v-if="favs.length" class="bmrow">
        <button
          v-for="f in favs"
          :key="f.url"
          class="bm"
          :class="{ on: f.url === addr }"
          :title="f.url"
          @click="openTab(f.url)"
        >
          {{ f.name }}
        </button>
      </div>
      <div class="row">
        <div class="addrwrap">
          <input
            v-model="addr"
            class="addr"
            spellcheck="false"
            :placeholder="tr('live.addrPh')"
            @keydown.enter="go"
          />
          <div v-show="loading" class="loadbar" />
        </div>
        <button
          class="op"
          :class="{ 'pad-on': padMode === 1 }"
          :title="tr('live.padTip')"
          @click="togglePad"
        >
          <WIcon name="gamepad" :size="15" />
        </button>
        <button class="op" :title="tr('live.lockTip')" @click="applyLock(true)">
          <WIcon name="lock" :size="15" />
        </button>
        <button
          class="op"
          :class="{ fav: isFav }"
          :title="isFav ? tr('live.clearFav') : tr('live.favTip')"
          @click="toggleFav"
        >
          <WIcon :name="isFav ? 'starFill' : 'star'" :size="15" />
        </button>
        <button class="op danger" :title="tr('live.closeAll')" @click="closeAll">
          <WIcon name="close" :size="15" />
        </button>
      </div>
    </template>
    <!-- 手柄控制提示：开启结果 / 后端拒绝原因（3s 自动消失，不挡操作） -->
    <div v-if="toast" class="padtoast">{{ toast }}</div>
  </div>
</template>

<style scoped>
.tb {
  position: relative;
  width: 100%;
  height: 100vh;
  display: flex;
  flex-direction: column;
  justify-content: center;
  gap: 4px;
  padding: 5px 7px;
  background: rgba(33, 35, 38, 0.98);
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 12px;
  /* 不加外阴影：阴影会填充透明窗口四角的圆角缺口，看起来下面是直角 */
}
.tb:not(.single) {
  /* 浏览器贴合态：满幅与下方页面连成一体——圆角面板悬浮在透明窗口里
     会在左右露出「缺口」，看起来像两块拼错位的皮。顶部两角用 CSS 圆角
     （窗口透明，圆角处透出桌面），底部直角与页面严丝合缝 */
  border-radius: 12px 12px 0 0;
  border-left: none;
  border-right: none;
  border-top: none;
  border-bottom: 1px solid rgba(255, 255, 255, 0.1); /* 与页面的分隔线 */
  justify-content: flex-start;
}
.tb.single {
  /* 宽度随内容（fitPickerWidth 据此实测窗口宽）：平台/收藏再多也不挤成叠字 */
  width: max-content;
  flex-direction: row;
  align-items: center;
  gap: 10px;
  padding: 0 12px;
}
.vsep {
  flex: none;
  width: 1px;
  height: 22px;
  background: rgba(255, 255, 255, 0.14);
  margin: 0 2px;
}
.row {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  min-height: 0;
}
.row:last-child {
  height: 30px;
  flex: none;
}
.row:first-child {
  height: 26px;
  flex: none;
}

/* 平台快捷入口 */
.hint {
  padding: 0 6px 0 4px;
  font-size: 12px;
  color: rgba(255, 255, 255, 0.45);
  flex: none;
}
.plat {
  /* 自然宽 + 可收缩：窗口被显示器宽度夹住时按内容占比收缩并省略号，
     绝不让文字溢出胶囊（点标记始终留在背景内） */
  flex: 0 1 auto;
  height: 34px;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  border: none;
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.06);
  color: #d6d8db;
  font-size: 13px;
  padding: 0 14px;
  cursor: pointer;
  transition: background 0.12s ease, color 0.12s ease;
}
/* 按钮内文字：独占可收缩位，超出即省略（flex 容器上 text-overflow 不生效） */
.pname {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}
.plat:hover {
  background: rgba(255, 255, 255, 0.13);
  color: #fff;
}
.plat:active {
  background: rgba(255, 255, 255, 0.18);
}
.dot {
  flex: none;
  width: 8px;
  height: 8px;
  border-radius: 50%;
}
/* 平台选择屏上的收藏入口：强调色高亮，与平台按钮区分 */
.plat.fav {
  background: color-mix(in srgb, var(--accent, #3a6df0) 22%, transparent);
  color: #fff;
}
.plat.fav:hover {
  background: color-mix(in srgb, var(--accent, #3a6df0) 32%, transparent);
}
.fav-dot {
  display: flex;
  align-items: center;
  justify-content: center;
  background: transparent !important;
  color: var(--accent, #ffd75e);
}
.favx {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 14px;
  height: 14px;
  margin-left: 2px;
  border-radius: 4px;
  color: rgba(255, 255, 255, 0.55);
}
.favx:hover {
  background: rgba(255, 255, 255, 0.16);
  color: #fff;
}

/* 标签页 */
.tabstrip {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
  overflow-x: auto;
  scrollbar-width: none;
}
.tabstrip::-webkit-scrollbar {
  display: none;
}
.tab {
  flex: none;
  display: flex;
  align-items: center;
  gap: 5px;
  max-width: 180px;
  min-width: 0;
  height: 24px;
  padding: 0 4px 0 10px;
  border-radius: 7px;
  border: 1px solid transparent;
  background: rgba(255, 255, 255, 0.05);
  color: #cfd2d6;
  font-size: 12px;
  cursor: pointer;
  user-select: none;
}
.tab:hover {
  background: rgba(255, 255, 255, 0.09);
}
.tab.on {
  background: rgba(255, 255, 255, 0.13);
  border-color: rgba(255, 255, 255, 0.12);
  color: #fff;
}
.tname {
  max-width: 130px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tclose {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  border-radius: 4px;
  color: rgba(255, 255, 255, 0.5);
}
.tclose:hover {
  background: rgba(255, 255, 255, 0.16);
  color: #fff;
}

/* 地址栏 */
.addrwrap {
  position: relative;
  flex: 1;
  min-width: 0;
  display: flex;
}
.addr {
  flex: 1;
  min-width: 0;
  height: 30px;
  padding: 0 10px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 8px;
  background: rgba(0, 0, 0, 0.35);
  color: #e8eaed;
  font-size: 13px;
  outline: none;
  user-select: text;
  -webkit-user-select: text;
}
.addr::placeholder {
  color: rgba(255, 255, 255, 0.3);
}
.addr:focus {
  border-color: rgba(58, 109, 240, 0.8);
}
/* 加载中：地址栏底沿跑马灯细进度条（不定进度，导航结束即消失） */
.loadbar {
  position: absolute;
  left: 1px;
  right: 1px;
  bottom: 1px;
  height: 2px;
  overflow: hidden;
  border-radius: 0 0 7px 7px;
  pointer-events: none;
}
.loadbar::before {
  content: "";
  position: absolute;
  top: 0;
  bottom: 0;
  width: 38%;
  border-radius: 2px;
  background: #3a6df0;
  animation: egb-loadbar 1.1s ease-in-out infinite;
}
@keyframes egb-loadbar {
  from {
    transform: translateX(-110%);
  }
  to {
    transform: translateX(380%);
  }
}

/* 操作按钮 */
.op {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: #d6d8db;
  cursor: pointer;
  transition: background 0.12s ease, color 0.12s ease;
}
/* 拖动手柄：按住即可拖动整个直播窗口 */
.grip {
  cursor: grab;
}
.grip:active {
  cursor: grabbing;
}
.op.slim {
  width: 24px;
  height: 24px;
}
.op:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #fff;
}
.op.danger:hover {
  background: rgba(224, 74, 74, 0.24);
  color: #ff8a8a;
}
.op.fav {
  color: var(--accent, #ffd75e);
}
/* 手柄控制激活态：强调色底，一眼看出「现在摇杆在控制浏览器」 */
.op.pad-on {
  background: color-mix(in srgb, var(--accent, #3a6df0) 34%, transparent);
  color: #fff;
}
/* 提示条：绝对定位浮在工具条内部下沿（窗口是固定尺寸的，浮到窗外会被直接裁掉；
   absolute 不参与 scrollHeight 测量，不会撑高窗口）。3s 自动消失，遮挡可接受 */
.padtoast {
  position: absolute;
  left: 50%;
  bottom: 3px;
  transform: translateX(-50%);
  max-width: 92%;
  padding: 4px 12px;
  border-radius: 8px;
  background: rgba(18, 20, 24, 0.94);
  border: 1px solid rgba(255, 255, 255, 0.14);
  color: #eef0f3;
  font-size: 12px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  pointer-events: none;
  z-index: 10;
}

/* 手柄焦点环：gp 导航的可见高亮（无此样式时手柄焦点不可见 =「选不了」） */
:deep(.gp-focus),
.gp-focus {
  outline: 2px solid var(--accent, #4a8fe7);
  outline-offset: 1px;
  border-radius: 6px;
}

/* 收藏栏：浏览器模式下的可点击书签条 */
.bmrow {
  display: flex;
  align-items: center;
  gap: 4px;
  overflow-x: auto;
  scrollbar-width: none;
  padding: 0 2px;
  min-height: 24px;
}
.bmrow::-webkit-scrollbar { display: none; }
.bm {
  flex: none;
  padding: 3px 9px;
  border-radius: 6px;
  border: 1px solid transparent;
  background: rgba(255, 255, 255, 0.06);
  color: #cfd2d6;
  font-size: 11.5px;
  cursor: pointer;
  white-space: nowrap;
  transition: background 0.12s ease, color 0.12s ease;
}
.bm:hover {
  background: rgba(255, 255, 255, 0.12);
  color: #fff;
}
.bm.on {
  border-color: var(--accent-strong, rgba(90, 140, 255, 0.6));
  color: var(--accent-text, #cfe0ff);
}
</style>