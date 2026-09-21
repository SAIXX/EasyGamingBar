<script setup lang="ts">
// 快捷指令抽屉 / 首次提示：独立透明小窗。
// 不用毛玻璃（本窗口不调 applySelfGlass），所以不会出现背景板黑底；窗口
// 本身透明，只在被点图标正下方浮出内容。
// - actions 模式：竖排主题色图标，关机需按住 1 秒
// - tip 模式：首次启动的 Steam 手柄提示（只告知，绝不修改用户设置）
import { onBeforeUnmount, onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize, PhysicalPosition } from "@tauri-apps/api/dpi";
import { emit, listen } from "@tauri-apps/api/event";
import WIcon from "../shared/WIcon.vue";
import { bindAccent } from "../shared/skins";
import {
  loadConfig,
  quickCloseGame,
  quickOpenScreenshotDir,
  quickScreenshotFull,
  quickShutdown,
  quickShowDesktop,
} from "../shared/api";
import { t } from "../shared/i18n";
import { startGamepadNav } from "../shared/gamepad";

type Mode = "actions" | "tip";

const mode = ref<Mode>("actions");
const shown = ref(false);
const shooting = ref(false); // 截图进行中（框选阶段）
const holdProgress = ref(0); // 0-100，驱动关机图标底部的进度条
const HOLD_MS = 1000;
let holdRaf = 0;
let holdStart = 0;
let hideTimer: ReturnType<typeof setTimeout> | null = null;
let tipTimer: ReturnType<typeof setTimeout> | null = null;
let saveDir = ""; // 截图保存目录（设置中心「通用 → 文件保存地址」）

const ACTIONS = [
  { key: "shot", icon: "camera", tipKey: "qd.shot" },
  { key: "desktop", icon: "home", tipKey: "qd.desktop" },
  { key: "shutdown", icon: "power", tipKey: "qd.shutdown" },
  { key: "close", icon: "xOctagon", tipKey: "qd.close" },
  { key: "dir", icon: "image", tipKey: "qd.dir" },
];

/** 结果提示借用 tooltip 小窗：本窗口保持窄条，不在游戏画面上挡一大块 */
async function flash(text: string) {
  const win = getCurrentWindow();
  const pos = await win.outerPosition();
  const size = await win.outerSize();
  await emit("tip://show", {
    text,
    x: pos.x + Math.round(size.width / 2),
    y: pos.y + size.height + 6,
  });
  if (tipTimer) clearTimeout(tipTimer);
  tipTimer = setTimeout(() => void emit("tip://hide"), 3200);
}

async function doScreenshot() {
  if (shooting.value) return;
  shooting.value = true;
  try {
    const r = await quickScreenshotFull(saveDir);
    if (r.saved) await flash(t("qd.savedTo", { path: r.path ?? "" }));
    else await flash(t("qd.cancelled"));
  } catch (e) {
    await flash(String(e));
  } finally {
    shooting.value = false;
  }
}

async function doShowDesktop() {
  try {
    await quickShowDesktop();
    await flash(t("qd.desktopDone"));
  } catch (e) {
    await flash(String(e));
  }
}

/** 关机：按住满 1 秒才发指令（无二次确认页） */
async function doShutdown() {
  try {
    await quickShutdown();
    await flash(t("qd.shutdownSent"));
  } catch (e) {
    await flash(String(e));
  }
}

async function doCloseGame() {
  try {
    const title = await quickCloseGame();
    await flash(t("qd.closedTarget", { title }));
  } catch (e) {
    await flash(String(e));
  }
}

async function doOpenShotDir() {
  try {
    const dir = await quickOpenScreenshotDir(saveDir);
    await flash(t("qd.openedDir", { dir }));
  } catch (e) {
    await flash(String(e));
  }
}

function run(key: string) {
  if (key === "shot") void doScreenshot();
  else if (key === "desktop") void doShowDesktop();
  else if (key === "dir") void doOpenShotDir();
  // shutdown / close 走按住确认路径，见 startHold
}

/* ---- 危险动作「按住 1 秒」（关机 / 强制关闭前台）---- */

function holdTick(now: number) {
  const p = Math.min(100, ((now - holdStart) / HOLD_MS) * 100);
  holdProgress.value = p;
  if (p >= 100) {
    holdRaf = 0;
    holdProgress.value = 0;
    const act = holdAction.value;
    holdAction.value = "";
    if (act === "close") void doCloseGame();
    else void doShutdown();
    return;
  }
  holdRaf = requestAnimationFrame(holdTick);
}

// 用 ref：进度条只画在**正在按住**的那个按钮上（两个长按动作共用一份进度值）
const holdAction = ref("");
const holdable = (key: string) => key === "shutdown" || key === "close";
function startHold(key: string) {
  cancelHold();
  holdAction.value = key;
  void flash(t("qd.holdHint"));
  holdStart = performance.now();
  holdRaf = requestAnimationFrame(holdTick);
}

/* 手柄长按协议（src/shared/gamepad.ts）：元素带 data-gp-hold 时，手柄 A 的
 * 按下沿派发 gp-hold-start、松开/焦点丢失派发 gp-hold-end——因为手柄事件没有
 * pointer 语义，gamepad.ts 不会走 click（上面 @click 对长按动作是空操作）。
 * 与 GpSelect.vue 的 gp-back 同一套做法：手动挂监听（Vue 的 @事件名 对
 * 带连字符的自定义事件不可靠），事件冒泡，用 closest 找回按钮。 */
function onGpHoldStart(e: Event) {
  const btn = (e.target as HTMLElement | null)?.closest?.<HTMLElement>("[data-gp-hold]");
  const key = btn?.dataset.key ?? "";
  if (btn && holdable(key)) startHold(key);
}

function cancelHold() {
  if (holdRaf) {
    cancelAnimationFrame(holdRaf);
    holdRaf = 0;
  }
  holdProgress.value = 0;
}

/* ---- 显示 / 隐藏 ---- */

async function onShow(p: { mode?: Mode; x: number; y: number }) {
  const win = getCurrentWindow();
  mode.value = p.mode === "tip" ? "tip" : "actions";
  const w = mode.value === "tip" ? 340 : 60;
  const h = mode.value === "tip" ? 150 : 220;
  await win.setSize(new LogicalSize(w, h));
  const size = await win.outerSize();
  await win.setPosition(
    new PhysicalPosition(Math.round(p.x - size.width / 2), p.y)
  );
  shown.value = true;
  await win.show();
  if (mode.value === "tip") {
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = setTimeout(hide, 20_000); // 提示卡 20 秒自动收起
  }
}

function hide() {
  if (!shown.value) return;
  shown.value = false;
  cancelHold();
  if (hideTimer) {
    clearTimeout(hideTimer);
    hideTimer = null;
  }
  void emit("tip://hide");
  void emit("quick://closed"); // 悬浮条据此复位按钮点亮态
  // 立即隐藏窗口：延迟淡出期间 is_visible 仍为 true，手柄路由会把它当成展开态窜键
  // （曾把「强制关闭」发给前台 PowerShell），收起改为瞬时，不做淡出
  void getCurrentWindow().hide();
}

let unlisten: Array<() => void> = [];
// 手柄：B 收起抽屉；A/摇杆操作动作按钮（按钮均为原生 button，天然可选中）

onMounted(async () => {
  void loadConfig().then((c) => {
    saveDir = String(c.settings["saveDir"] ?? "");
    bindAccent(c.settings); // 图标跟随皮肤强调色
  });
  unlisten.push(
    await listen<{ mode?: Mode; x: number; y: number }>("quick://show", (e) => {
      void onShow(e.payload);
    })
  );
  unlisten.push(await listen("quick://hide", () => hide()));
  // 强调色跟随皮肤换色（挂载时只绑过一次，换肤后这里同步）
  unlisten.push(
    await listen("config://updated", async () => {
      const c = await loadConfig().catch(() => null);
      if (c) {
        saveDir = String(c.settings["saveDir"] ?? "");
        bindAccent(c.settings);
      }
    })
  );
  // 手柄：摇杆/A 在动作按钮间选择确认，B 收起抽屉
  unlisten.push(startGamepadNav({ onBack: hide }));
  // 手柄「按住 A 确认」：挂在 document 上（按钮随 mode 的 v-if 反复创建销毁，
  // 挂元素上会在切模式后失效）
  document.addEventListener("gp-hold-start", onGpHoldStart);
  document.addEventListener("gp-hold-end", cancelHold);
});

onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
  document.removeEventListener("gp-hold-start", onGpHoldStart);
  document.removeEventListener("gp-hold-end", cancelHold);
  cancelHold();
  if (hideTimer) clearTimeout(hideTimer);
  if (tipTimer) clearTimeout(tipTimer);
});
</script>

<template>
  <div class="wrap" :class="{ show: shown }">
    <!-- 指令模式：竖排透明主题色图标 -->
    <div v-if="mode === 'actions'" class="col">
      <button
        v-for="a in ACTIONS"
        :key="a.key"
        class="qaction"
        :class="{ holding: a.key === holdAction && holdProgress > 0 }"
        :data-gp-initial="a.key === 'shot' ? '' : undefined"
        :disabled="a.key === 'shot' && shooting"
        :data-key="a.key"
        :data-gp-hold="holdable(a.key) ? '' : undefined"
        @pointerdown="holdable(a.key) ? startHold(a.key) : undefined"
        @pointerup="holdable(a.key) ? cancelHold : undefined"
        @pointerleave="holdable(a.key) ? cancelHold : undefined"
        @pointercancel="holdable(a.key) ? cancelHold : undefined"
        @click="holdable(a.key) ? undefined : run(a.key)"
        @mouseenter="flash(t(a.tipKey))"
        @mouseleave="void emit('tip://hide')"
      >
        <WIcon :name="a.icon" :size="19" />
        <span
          v-if="holdable(a.key)"
          class="hold-bar"
          :style="{ width: (a.key === holdAction ? holdProgress : 0) + '%' }"
        ></span>
      </button>
    </div>

    <!-- 提示模式：首次启动的 Steam 手柄提示（只告知，不改用户设置） -->
    <div v-else class="tip-card">
      <p class="ft-text" v-html="t('qd.tipBody')"></p>
      <button class="ft-btn" data-gp-initial @click="hide">{{ t("qd.tipOk") }}</button>
    </div>
  </div>
</template>

<style scoped>
.wrap {
  width: 100vw;
  height: 100vh;
  display: flex;
  justify-content: center;
  opacity: 0;
  transition: opacity 0.16s ease;
}
.wrap.show {
  opacity: 1;
}

/* 竖排图标：完全透明底，颜色即主题强调色（跟随皮肤换色）。
   顶部留白须够大：窗口锚在悬浮条下方 8px，再贴上去第一个图标
   （截图）就跟悬浮条粘连了 */
.col {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding-top: 12px;
}
.qaction {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 36px;
  height: 36px;
  border: none;
  border-radius: 9px;
  background: transparent;
  color: var(--accent, #4a72e8);
  cursor: pointer;
  transition: color 0.13s ease, text-shadow 0.13s ease,
    transform 0.18s cubic-bezier(0.34, 1.56, 0.64, 1);
}
.qaction.gp-focus,
.qaction:hover {
  color: var(--accent-hover, #5a80f0);
  text-shadow: 0 0 10px var(--accent-strong, rgba(90, 140, 255, 0.6));
  transform: translateY(-1px);
}
.qaction:active {
  transform: scale(0.88);
  transition-duration: 0.08s;
}
.qaction:disabled {
  opacity: 0.55;
  cursor: wait;
}
.qaction.holding {
  transform: scale(1.08);
  color: var(--accent-hover, #5a80f0);
}
/* 关机按住进度条：图标底部一条从左向右充满的细条 */
.hold-bar {
  position: absolute;
  left: 50%;
  bottom: 3px;
  transform: translateX(-50%);
  height: 3px;
  width: 0;
  border-radius: 2px;
  background: var(--accent, #4a72e8);
  pointer-events: none;
}

/* 首次提示卡 */
.tip-card {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  width: max-content;
  max-width: 330px;
  margin-top: 4px;
  padding: 10px 12px;
  border: 1px solid var(--panel-border);
  border-radius: 10px;
  background: var(--panel-bg);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
}
.ft-text {
  font-size: 12px;
  line-height: 1.6;
  color: #e8eaed;
}
.ft-text b {
  font-weight: 600;
  color: var(--accent, #4a72e8);
}
.ft-path {
  color: rgba(255, 255, 255, 0.7);
}
.ft-note {
  font-size: 11px;
  color: rgba(255, 255, 255, 0.45);
}
.ft-btn {
  flex: none;
  padding: 5px 10px;
  border: 1px solid var(--panel-border);
  border-radius: 7px;
  background: var(--accent-faint, rgba(90, 140, 255, 0.14));
  color: var(--accent, #4a72e8);
  font-size: 12px;
  cursor: pointer;
}
.ft-btn:hover {
  background: var(--accent-mid, rgba(90, 140, 255, 0.32));
  color: var(--accent-hover, #5a80f0);
}
</style>
