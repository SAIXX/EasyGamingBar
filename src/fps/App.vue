<script setup lang="ts">
// 游戏内 FPS 悬浮窗：订阅 Rust 的性能快照显示帧数（UI 全部隐藏后依然持续工作）。
// 帧率来自注入链路（egb_hook.dll 在游戏进程内量帧时间，perf.rs 聚合后推送）。
//
// **掉帧优化（重要）**：这个小窗背后是一整个 Chromium 渲染进程 + 合成器，每秒
// 走一遍 emit → JS → Vue 依赖收集 → vnode diff/patch → 重绘 的代价并不小，
// 而游戏满负荷时这点开销会直接体现在帧时间上。所以这里：
//   ① Rust 侧已做「内容没变就不推送」（见 perf.rs 的 overlay_signature 去重）；
//   ② 收到后**不走 Vue 响应式**，直接写 DOM 的 textContent，且文本相同就不赋值
//      （不赋值 = 不触发 layout/paint）。Vue 只在挂载时渲染一次静态骨架。
import { onBeforeUnmount, onMounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import { t } from "../shared/i18n";

const elValue = ref<HTMLElement | null>(null);
const elLowRow = ref<HTMLElement | null>(null);
const elLow = ref<HTMLElement | null>(null);
const elGpu = ref<HTMLElement | null>(null);
const elVram = ref<HTMLElement | null>(null);
const unlisten: Array<() => void> = [];

/** 只在文本真的变化时才写 DOM：赋值本身就会让浏览器标脏并重绘这一行 */
function setText(el: HTMLElement | null, txt: string) {
  if (el && el.textContent !== txt) el.textContent = txt;
}
function show(el: HTMLElement | null, on: boolean) {
  if (!el) return;
  const want = on ? "" : "none";
  if (el.style.display !== want) el.style.display = want;
}

/** 主数字 = 游戏渲染帧率（注入 DLL 在 Present 里量帧时间，与游戏内计数器口径一致） */
let lastFps: number | null = null;
/** 1% Low：注入 DLL 的滑动窗口分位统计 */
let lastLow: number | null = null;
let lastGpu: number | null = null;
let lastVramUsed: number | null = null;
let lastVramTotal: number | null = null;

function render() {
  setText(elValue.value, lastFps == null ? "--" : String(lastFps));
  elValue.value?.classList.toggle("na", lastFps == null);
  // 1% Low 没测到（窗口还没攒满 / 未注入）就整行不画（默认 display:none）
  show(elLowRow.value, lastLow != null);
  if (lastLow != null) setText(elLow.value, t("fps.low", { v: lastLow }));
  setText(elGpu.value, lastGpu == null ? "--" : Math.round(lastGpu) + "%");
  setText(
    elVram.value,
    lastVramUsed == null
      ? "--"
      : lastVramTotal == null
        ? `${(lastVramUsed / 1024).toFixed(1)}G`
        : `${(lastVramUsed / 1024).toFixed(1)}/${(lastVramTotal / 1024).toFixed(1)}G`,
  );
}

onMounted(async () => {
  unlisten.push(
    // Rust 聚合线程推送（已按内容去重；另有 5s 心跳兜底）
    await listen<{
      game: {
        fps: number | null;
        fps_low?: number | null;
      } | null;
      gpu?: number | null;
      vram_used?: number | null;
      vram_total?: number | null;
    }>("perf://status", (e) => {
      const g = e.payload.game;
      // 帧率只认前台游戏的读数（注入目标就是前台游戏，Rust 侧已核对归属），
      // 没注入成功就是 null——这里不给任何兜底，宁可显示 "--"
      lastFps = g ? g.fps : null;
      lastLow = g?.fps_low ?? null;
      lastGpu = e.payload.gpu ?? null;
      lastVramUsed = e.payload.vram_used ?? null;
      lastVramTotal = e.payload.vram_total ?? null;
      render();
    })
  );
});
onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
  unlisten.length = 0;
});
</script>

<template>
  <div class="overlay">
    <div class="row">
      <span class="label">FPS</span>
      <span ref="elValue" class="value na">--</span>
    </div>
    <div ref="elLowRow" class="low" style="display: none">
      <span ref="elLow"></span>
    </div>
    <div class="sub">
      <span>GPU <span ref="elGpu">--</span></span>
      <span class="dot">·</span>
      <span>VRAM <span ref="elVram">--</span></span>
    </div>
  </div>
</template>

<style scoped>
.overlay {
  width: 100vw;
  height: 100vh;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 5px;
  background: rgba(12, 13, 15, 0.55);
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 10px;
  /* 隔离这个小窗的布局与绘制：内容变化不再向上波及整个文档的合成/重排 */
  contain: layout paint;
}
.row {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 9px;
}
.label {
  font-size: 13px;
  font-weight: 600;
  color: rgba(255, 255, 255, 0.75);
}
.value {
  font-size: 24px;
  font-weight: 700;
  line-height: 1;
  color: #fff;
  /* 等宽数字：帧数跳变时不会左右抖动，也就不会连带整行重排 */
  font-variant-numeric: tabular-nums;
}
.value.na {
  font-size: 18px;
  color: rgba(255, 255, 255, 0.4);
}
/* 1% Low 副行（RTSS 式）：注入 DLL 统计到时才显示 */
.low {
  font-size: 11px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  color: rgba(255, 255, 255, 0.7);
  white-space: nowrap;
}
/* RTSS 式副行：GPU 占用 + 显存用量 */
.sub {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 11px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  color: rgba(255, 255, 255, 0.78);
  white-space: nowrap;
}
.dot {
  color: rgba(255, 255, 255, 0.35);
}
</style>
