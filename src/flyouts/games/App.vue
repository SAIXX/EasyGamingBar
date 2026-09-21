<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { emit, listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import WIcon from "../../shared/WIcon.vue";
import { bindAccent, skinStyle } from "../../shared/skins";
import { startGamepadNav } from "../../shared/gamepad";
import { enterOnShow, notifyReady } from "../../shared/enter";
import {
  extractExeIcon,
  launchProcess,
  loadConfig,
  trackGameSession,
  plain,
  steamPlaytime,
  updateConfig,
  type AppConfig,
} from "../../shared/api";
import { t } from "../../shared/i18n";

const config = ref<AppConfig>({ settings: {}, games: [], widgets: {} });
const panelSkin = computed(() => skinStyle(config.value.settings, 0.18));
const iconCache = new Map<string, string>();
const iconVersion = ref(0);
/** Steam 游戏时长（appid → 分钟数），打开面板时读取一次 */
const playtime = ref<Record<string, number>>({});

function playtimeText(g: { steamAppId?: string }): string | null {
  if (!g.steamAppId) return null;
  const m = playtime.value[g.steamAppId];
  if (!m) return null;
  return m >= 60 ? `${(m / 60).toFixed(1).replace(/\.0$/, "")} 小时` : `${m} 分钟`;
}
const rootEl = ref<HTMLElement | null>(null);

onMounted(() => {
  if (rootEl.value) enterOnShow(rootEl.value);
  notifyReady(); // 首帧就绪 → 悬浮条才 show 本窗口
  // 手柄：B 返回 = 关闭面板
  unlisten.push(startGamepadNav({ onBack: close }));
});

let saveTimer: ReturnType<typeof setTimeout> | null = null;
/** 本面板只改 games（排序/增删/图标）：只提交 games，不动 settings/widgets */
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

function thumbSrc(exePath: string, iconPath?: string): string | null {
  const p = iconPath || iconCache.get(exePath) || null;
  return p ? convertFileSrc(p) : null;
}

async function ensureIcons() {
  for (const g of config.value.games) {
    if (g.iconPath || iconCache.has(g.exePath)) continue;
    iconCache.set(g.exePath, "");
    try {
      const p = await extractExeIcon(g.exePath);
      iconCache.set(g.exePath, p);
      g.iconPath = p;
      iconVersion.value++;
      persist();
    } catch {
      iconCache.delete(g.exePath);
    }
  }
}

/* 点击启动：先给缩略图一个放大脉冲，动画过后再关面板（直接关会看不到动画） */
const launchingIdx = ref(-1);

async function launch(i: number) {
  const g = config.value.games[i];
  if (!g || launchingIdx.value >= 0) return;
  launchingIdx.value = i;
  setTimeout(close, 460);
  try {
    // Steam 游戏走官方协议启动，保证 DRM 与 Steam 运行时正常
    await launchProcess(
      g.steamAppId ? `steam://rungameid/${g.steamAppId}` : g.exePath
    );
    void trackGameSession(g.exePath).catch(() => {});
  } catch (e) {
    console.error(e);
  }
}

let dragFrom = -1;
function onDragStart(i: number, e: DragEvent) {
  dragFrom = i;
  e.dataTransfer?.setData("text/plain", String(i));
}
function onDragOver(e: DragEvent) {
  e.preventDefault();
}
function onDrop(i: number, e: DragEvent) {
  e.preventDefault();
  if (dragFrom < 0 || dragFrom === i) return;
  const list = config.value.games;
  const [moved] = list.splice(dragFrom, 1);
  list.splice(i, 0, moved);
  dragFrom = -1;
  iconVersion.value++;
  persist();
}

function removeGame(i: number) {
  config.value.games.splice(i, 1);
  persist();
}

async function openSettingsCenter() {
  // 设置页常驻保留上次浏览位置：添加游戏是明确意图，先广播切到「游戏管理」再显示
  await emit("settings://nav", "games");
  const settings = await WebviewWindow.getByLabel("settings");
  if (settings) {
    await settings.show();
    await settings.setFocus();
  } else {
    const win = new WebviewWindow("settings", {
      url: "settings.html",
      title: t("win.settings"),
      width: 880,
      height: 600,
      center: true,
      resizable: false,
      decorations: false,
      transparent: true,
      shadow: false,
      dragDropEnabled: false,
    });
    win.once("tauri://created", async () => {
      await win.show();
      await win.setFocus();
    });
  }
  await close();
}

async function close() {
  // 隐藏复用而非销毁：面板页面常驻（含图标/时长缓存），重开瞬时
  await getCurrentWindow().hide();
}

let unlisten: Array<() => void> = [];
onMounted(async () => {
  config.value = await loadConfig();
  bindAccent(config.value.settings); // 跟随皮肤主题色（默认经典蓝）
  await ensureIcons();
  // Steam 游戏时长（读 userdata/<账号>/config/localconfig.vdf）
  const ids = config.value.games
    .map((x) => x.steamAppId)
    .filter((x): x is string => !!x);
  if (ids.length) {
    try {
      const rows = await steamPlaytime(ids);
      const map: Record<string, number> = {};
      for (const r of rows) map[r.appid] = r.minutes;
      playtime.value = map;
    } catch {
      /* 无时长数据不阻塞面板 */
    }
  }
  unlisten.push(
    await listen("config://updated", async () => {
      config.value = await loadConfig();
      bindAccent(config.value.settings);
    })
  );
  const off = await getCurrentWindow().onFocusChanged(({ payload }) => {
    if (!payload) close();
  });
  unlisten.push(off);
});
onBeforeUnmount(() => unlisten.forEach((f) => f()));
</script>

<template>
  <div ref="rootEl" class="panel" :style="panelSkin">
    <header>
      <span class="title">{{ t("games.all", { n: config.games.length }) }}</span>
      <span class="hint">{{ t("games.hint") }}</span>
      <button class="x" @click="close"><WIcon name="close" :size="15" /></button>
    </header>

    <div class="grid">
      <div
        v-for="(g, i) in config.games"
        :key="g.id"
        class="cell"
        draggable="true"
        data-nav
        role="button"
        :class="{ launching: i === launchingIdx }"
        :title="g.name"
        @click="launch(i)"
        @dragstart="onDragStart(i, $event)"
        @dragover="onDragOver"
        @drop="onDrop(i, $event)"
        @dragend="dragFrom = -1"
      >
        <button
          class="rm"
          :title="t('games.remove')"
          draggable="false"
          @mousedown.stop
          @click.stop="removeGame(i)"
         data-gp-skip>
          <WIcon name="close" :size="10" />
        </button>
        <div class="thumb">
          <img
            v-if="thumbSrc(g.exePath, g.iconPath) && iconVersion >= 0"
            :src="thumbSrc(g.exePath, g.iconPath)!"
            alt=""
            draggable="false"
          />
          <span v-else class="ph">{{ g.name.slice(0, 1).toUpperCase() }}</span>
        </div>
        <div class="name">{{ g.name }}</div>
        <!-- 常驻占位：无时长也要撑住行高，否则 Steam 游戏（有时长）比
             本地游戏整行高一截，格子大小不一致 -->
        <div class="ptime" :title="g.name">{{ playtimeText(g) }}</div>
      </div>

      <div class="cell add" data-nav role="button" @click="openSettingsCenter">
        <div class="thumb add-t"><WIcon name="plus" :size="22" /></div>
        <div class="name">{{ t("games.add") }}</div>
      </div>
    </div>

    <div v-if="config.games.length === 0" class="empty">
      {{ t("games.empty") }}
    </div>
  </div>
</template>

<style scoped>
.panel {
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
  /* 弹出入场：等窗口可见后由 enterOnShow 加 .enter 播放 */
  opacity: 0;
  transform: translateY(12px) scale(0.96);
  transition: opacity 0.22s ease, transform 0.32s cubic-bezier(0.3, 1.35, 0.4, 1);
}
.panel.enter {
  opacity: 1;
  transform: none;
}

header {
  flex: none;
  display: flex;
  align-items: center;
  gap: 10px;
  height: 44px;
  padding: 0 10px 0 16px;
}
.title {
  font-size: 13.5px;
  font-weight: 600;
  color: #f2f3f5;
}
.hint {
  flex: 1;
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.35);
}
.x {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: rgba(255, 255, 255, 0.65);
  cursor: pointer;
}
.x:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #fff;
}

.grid {
  flex: 1;
  overflow-y: auto;
  display: grid;
  /* minmax(0,1fr) 强制三列永远等宽：裸 1fr = minmax(auto,1fr)，长游戏名
     （nowrap）会把所在列按内容撑宽，导致每排格子大小不一致 */
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 10px;
  padding: 4px 14px 14px;
  align-content: start;
}

.cell {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 7px;
  padding: 10px 6px;
  min-height: 127px; /* 统一格高：thumb64+gap+name16+时长占位14+内边距，加号格也不例外 */
  min-width: 0; /* 允许收缩到列宽，超长名字交给 name 省略号 */
  border-radius: 10px;
  border: 1px solid transparent;
  cursor: pointer;
  transition: background 0.15s ease, border-color 0.15s ease,
    transform 0.24s cubic-bezier(0.34, 1.56, 0.64, 1),
    box-shadow 0.2s ease;
}
/* 鼠标悬停与手柄焦点同一套主题色反馈（--accent* 随皮肤换色，见 bindAccent） */
.cell:hover,
.cell.gp-focus {
  background: var(--accent-faint);
  border-color: var(--accent-strong);
  transform: translateY(-2px);
}
.cell:hover .thumb,
.cell.gp-focus .thumb {
  transform: scale(1.08);
  box-shadow: 0 0 0 2px var(--accent-strong), 0 0 18px var(--accent-mid);
}
/* 点击启动：缩略图放大脉冲 + 主题色高亮，动画结束后才关面板 */
.cell.launching .thumb {
  animation: thumb-launch 0.5s cubic-bezier(0.34, 1.56, 0.64, 1);
}
@keyframes thumb-launch {
  0% {
    transform: scale(1);
  }
  40% {
    transform: scale(1.16);
    box-shadow: 0 0 0 3px var(--accent-strong), 0 0 22px var(--accent-mid);
  }
  100% {
    transform: scale(1);
    box-shadow: 0 0 0 0 transparent;
  }
}
.thumb {
  width: 64px;
  height: 64px;
  border-radius: 12px;
  overflow: hidden;
  background: rgba(255, 255, 255, 0.07);
  transition: transform 0.24s cubic-bezier(0.34, 1.56, 0.64, 1),
    box-shadow 0.2s ease;
}
.thumb img {
  width: 100%;
  height: 100%;
  object-fit: cover;
  pointer-events: none;
}
.imgwrap,
.ph {  display: flex;
  width: 100%;
  height: 100%;
  align-items: center;
  justify-content: center;
}
.ph {
  font-size: 24px;
  font-weight: 600;
  color: #fff;
  background: linear-gradient(135deg, #3a6df0, #7a3af0);
}
.name {
  max-width: 100%;
  font-size: 12px;
  color: #d6d8db;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.ptime {
  min-height: 14px; /* 空占位也撑住行高，保证所有格子等高 */
  margin-top: -3px;
  font-size: 10.5px;
  color: var(--accent-text, rgba(255, 255, 255, 0.55));
  font-variant-numeric: tabular-nums;
}
.cell.add .add-t {
  display: flex;
  align-items: center;
  justify-content: center;
  color: rgba(255, 255, 255, 0.5);
  border: 1px dashed rgba(255, 255, 255, 0.3);
  background: transparent;
}
.cell.add:hover .add-t,
.cell.add.gp-focus .add-t {
  color: var(--accent);
  border-color: var(--accent-strong);
}

.empty {
  padding: 10px 16px 16px;
  font-size: 12.5px;
  color: rgba(255, 255, 255, 0.4);
}
/* 悬停显示的移除按钮（手柄焦点同样可见） */
.rm {
  position: absolute;
  top: 2px;
  right: 2px;
  width: 20px;
  height: 20px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: none;
  border-radius: 6px;
  background: rgba(15, 16, 18, 0.85);
  color: rgba(255, 255, 255, 0.65);
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.12s ease, background 0.12s ease;
}
.cell:hover .rm,
.cell.gp-focus .rm {
  opacity: 1;
}
.rm:hover {
  background: rgba(224, 74, 74, 0.85);
  color: #fff;
}
</style>
