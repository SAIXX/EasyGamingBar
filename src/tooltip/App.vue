<script setup lang="ts">
// 悬浮条 tooltip 气泡小窗：免焦点 + 鼠标穿透（bar 侧 setIgnoreCursorEvents），
// 监听 tip://show / tip://hide 自行定位与展示，避免在悬浮条窗口内画气泡
// 遮挡游戏图标，也不会让毛玻璃背景板露出多余区域。
import { onBeforeUnmount, onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PhysicalPosition } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";

const text = ref("");
const show = ref(false);
const OUT_MS = 200; // 退场动画时长，播完再真隐藏窗口
const MAX_SHOWN_MS = 4000; // 兜底自动消失（与原生 tooltip 行为一致）
let hideTimer: ReturnType<typeof setTimeout> | null = null;
let outTimer: ReturnType<typeof setTimeout> | null = null;

// 窗口尺寸是固定的（420×32，气泡在里面居中），量一次就够；
// lastKey 用于去重：鼠标在同一控件内微动会反复收到 tip://show，
// 重复 setPosition / show 会让窗口反复重定位（一次 DWM 重合成），
// 表现就是悬停时悬浮条动画跟着顿一下。
let winWidth: number | null = null;
let winShown = false;
let lastKey = "";

async function onShow(p: { text: string; x: number; y: number }) {
  const win = getCurrentWindow();
  if (winWidth == null) winWidth = (await win.outerSize()).width;
  // 窗口水平中心对准悬浮元素中心（x/y 均为悬浮条换算好的物理坐标）
  const x = Math.round(p.x - winWidth / 2);
  const y = p.y;
  const key = `${p.text}|${x}|${y}`;
  if (key === lastKey && winShown) return;
  lastKey = key;
  await win.setPosition(new PhysicalPosition(x, y));
  text.value = p.text;
  show.value = true;
  if (!winShown) {
    await win.show();
    winShown = true;
  }
  if (hideTimer) clearTimeout(hideTimer);
  hideTimer = setTimeout(hide, MAX_SHOWN_MS);
}

function hide() {
  if (hideTimer) {
    clearTimeout(hideTimer);
    hideTimer = null;
  }
  if (!show.value) return;
  show.value = false;
  if (outTimer) clearTimeout(outTimer);
  outTimer = setTimeout(() => {
    void getCurrentWindow().hide();
    winShown = false;
    lastKey = ""; // 下次同文本也要重新定位（窗口藏过，位置可能已失效）
  }, OUT_MS);
}

let offHide: (() => void) | null = null;
onMounted(async () => {
  await listen<{ text: string; x: number; y: number }>("tip://show", (e) => {
    if (e.payload?.text) void onShow(e.payload);
  });
  offHide = await listen("tip://hide", () => hide());
});
onBeforeUnmount(() => {
  offHide?.();
  if (hideTimer) clearTimeout(hideTimer);
  if (outTimer) clearTimeout(outTimer);
});
</script>

<template>
  <div class="bubble" :class="{ show }">{{ text }}</div>
</template>

<style scoped>
.bubble {
  position: absolute;
  top: 2px;
  left: 50%;
  transform: translateX(-50%) translateY(-6px) scale(0.96);
  max-width: calc(100vw - 12px);
  padding: 3px 10px;
  border-radius: 7px;
  background: rgba(10, 12, 16, 0.92);
  border: 1px solid rgba(255, 255, 255, 0.14);
  color: #e8eaed;
  font-size: 12px;
  line-height: 1.5;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  opacity: 0;
  pointer-events: none;
  transition: opacity 0.14s ease, transform 0.2s cubic-bezier(0.3, 1.4, 0.4, 1);
}
.bubble.show {
  opacity: 1;
  transform: translateX(-50%);
}
</style>
