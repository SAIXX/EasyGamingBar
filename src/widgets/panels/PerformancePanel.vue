<script setup lang="ts">
// 性能组件：实时 CPU / GPU / VRAM / RAM 占用 + 游戏内 FPS 悬浮窗开关
// FPS 悬浮窗的定位与数据推送由 Rust 聚合线程每秒托管（perf::sync_overlay +
// perf://status 广播），面板只负责开关/位置设置与占用率展示——此前面板里还有
// 一套每秒 placeFpsOverlay + fps://tick 推流的旧引擎，与 Rust 侧同频打架
// （每秒双份定位日志、窗口反复重排），是悬浮窗「乱跳」的来源之一。
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { listen } from "@tauri-apps/api/event";
import WidgetShell from "../WidgetShell.vue";
import WIcon from "../../shared/WIcon.vue";
import {
  fpsOverlayPrefs,
  getPerfStatus,
  loadConfig,
  placeFpsOverlay,
  updateConfig,
  type FpsOverlayPos,
  type PerfGame,
  type PerfStatus,
} from "../../shared/api";
import { t } from "../../shared/i18n";

const OVERLAY_LABEL = "fps-overlay";

const st = ref<PerfStatus | null>(null);
const enabled = ref(false); // 游戏内 FPS 悬浮窗开关（持久化）
const pos = ref<FpsOverlayPos>("tl");
const showSettings = ref(false);

let unlistenPerf: (() => void) | null = null;

const metrics = computed(() => [
  { label: "CPU", icon: "cpu", value: st.value?.cpu },
  { label: "GPU", icon: "monitorAct", value: st.value?.gpu },
  { label: "VRAM", icon: "monitor", value: st.value?.vram },
  { label: "RAM", icon: "grid", value: st.value?.ram },
]);
const game = computed<PerfGame | null>(() => st.value?.game ?? null);
const shownFps = computed(() => game.value?.fps ?? null);
const pct = (v: number | null | undefined) =>
  v == null ? "--" : `${Math.round(v)}%`;

/* ---------------- 轮询 ---------------- */

async function poll() {
  try {
    const s = await getPerfStatus();
    st.value = s;
    await syncOverlay();
  } catch (e) {
    console.error("性能状态轮询失败", e);
  }
}

/* ---------------- FPS 悬浮窗生命周期 ---------------- */

/** 面板侧只剩一件事：开关关掉时立即隐藏悬浮窗（用户要即时反馈）。
 *  定位、显示、按秒推数全部归 Rust 聚合线程（perf::sync_overlay +
 *  perf://status 广播，悬浮窗页面自订阅）——此处的每秒 placeFpsOverlay +
 *  fps://tick 与 Rust 同频打架，是悬浮窗每秒重排/数值抖动的元凶之一。 */
async function syncOverlay() {
  if (enabled.value) return;
  const w = await WebviewWindow.getByLabel(OVERLAY_LABEL);
  if (w && (await w.isVisible().catch(() => false))) {
    await w.hide().catch(() => {});
  }
}

/* ---------------- 开关与设置（持久化） ---------------- */

async function persistPrefs() {
  try {
    // 只提交本面板拥有的两个 key，其余配置原样保留（Rust 侧落盘后广播 config://updated）
    await updateConfig({
      settings: { fpsOverlay: enabled.value, fpsOverlayPos: pos.value },
    });
  } catch (e) {
    console.error("保存 FPS 悬浮窗设置失败", e);
  }
}

async function toggleOverlay() {
  enabled.value = !enabled.value;
  await persistPrefs();
  // 悬浮窗由 Rust 聚合线程托管（perf::sync_overlay + perf://status 广播）：
  // Rust 侧 overlay_prefs 缓存 ≤5s 刷新，开关闭后立即接管定位与显隐。
  // 有前台游戏时立即补一次定位，让「打开开关 → 悬浮窗出现」无需等待。
  if (enabled.value && game.value) {
    try {
      await placeFpsOverlay(pos.value);
    } catch {
      /* 悬浮窗尚未预创建时失败属预期，Rust 侧 5s 内自建 */
    }
  }
}

const posOptions: { v: FpsOverlayPos; key: string }[] = [
  { v: "tl", key: "perf.tl" },
  { v: "tr", key: "perf.tr" },
  { v: "bl", key: "perf.bl" },
  { v: "br", key: "perf.br" },
];

async function setPos(p: FpsOverlayPos) {
  pos.value = p;
  await persistPrefs();
  // 位置立即生效
  if (enabled.value && game.value) {
    try {
      await placeFpsOverlay(p);
    } catch (e) {
      console.error(e);
    }
  }
}

/** 1% Low：注入链路（egb_hook.dll 的帧时间分位统计）提供 */
const lowsFps = computed(() => game.value?.fps_low ?? null);

/* ---------------- 生命周期 ---------------- */

onMounted(async () => {
  try {
    const cfg = await loadConfig();
    const prefs = fpsOverlayPrefs(cfg);
    enabled.value = prefs.enabled;
    pos.value = prefs.pos;
  } catch (e) {
    console.error(e);
  }
  // 先订阅每秒广播，再补一次初始拉取——即使拉取缓慢也不会空面板
  unlistenPerf = await listen<PerfStatus>("perf://status", (e) => {
    st.value = e.payload;
    void syncOverlay();
  });
  await poll();
});

onBeforeUnmount(() => {
  unlistenPerf?.();
  // 悬浮窗已交由 Rust 侧托管（perf::sync_overlay 每秒驱动）：面板关闭后依然常驻，
  // 只有关闭开关 / 退出游戏才会隐藏 —— 这里绝不能销毁窗口，否则 UI 一关 FPS 就没了
});
</script>

<template>
  <WidgetShell :title="t('perf.title')" icon="monitorAct">
    <template #ops>
      <button
        class="op"
        :class="{ on: showSettings }"
        :data-tip="t('perf.posTip')"
        @click="showSettings = !showSettings"
      >
        <WIcon name="sliders" :size="14" />
      </button>
    </template>

    <!-- 占用率：与显示面板同款卡片行 -->
    <div v-for="m in metrics" :key="m.label" class="row">
      <span class="label"><WIcon :name="m.icon" :size="15" /> {{ m.label }}</span>
      <span class="meter"><i :style="{ width: (m.value ?? 0) + '%' }" /></span>
      <span class="num">{{ pct(m.value) }}</span>
    </div>

    <!-- 前台游戏 -->
    <div class="row">
      <span class="label"><WIcon name="gamepad" :size="15" /> {{ t("perf.game") }}</span>
      <span class="gstate">
        <template v-if="game">
          {{ game.name }}{{ shownFps != null ? ` · ${shownFps} FPS` : t("perf.capturing") }}
        </template>
        <template v-else>{{ t("perf.launchForFps") }}</template>
      </span>
    </div>

    <!-- 1% Low：注入链路的帧时间分位统计 -->
    <div v-if="lowsFps != null" class="row">
      <span class="label"><WIcon name="zap" :size="15" /> {{ t("perf.low1") }}</span>
      <span class="gstate">{{ lowsFps }} FPS</span>
    </div>

    <!-- FPS 悬浮窗开关（与 HDR 开关同款） -->
    <div class="row">
      <span class="label"
        ><WIcon name="monitorAct" :size="15" /> {{ t("perf.fpsToggle") }}</span
      >
      <span class="gstate">{{ enabled ? t("common.enabled") : t("common.disabled") }}</span>
      <button
        class="toggle"
        :class="{ on: enabled }"
        :title="enabled ? t('perf.disableFpsTip') : t('perf.enableFpsTip')"
        @click="toggleOverlay"
      >
        <span class="knob" />
      </button>
    </div>

    <!-- 悬浮窗位置 -->
    <div v-if="enabled && showSettings" class="row col">
      <span class="label"><WIcon name="pin" :size="15" /> {{ t("perf.pos") }}</span>
      <div class="chips">
        <button
          v-for="o in posOptions"
          :key="o.v"
          class="chip"
          :class="{ on: pos === o.v }"
          @click="setPos(o.v)"
        >
          {{ t(o.key) }}
        </button>
      </div>
    </div>

    <div class="note">{{ t("perf.note") }}</div>
  </WidgetShell>
</template>

<style scoped>
/* 卡片行：与显示面板同款，保证各组件观感一致 */
.row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.04);
  margin-bottom: 8px;
  min-height: 46px;
}
.label {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  flex: none;
  width: 86px;
  font-size: 13px;
  color: #d6d8db;
}
.meter {
  flex: 1;
  height: 5px;
  border-radius: 3px;
  background: rgba(255, 255, 255, 0.12);
  overflow: hidden;
}
.meter i {
  display: block;
  height: 100%;
  border-radius: 3px;
  background: var(--accent);
  transition: width 0.3s ease;
}
.num {
  flex: none;
  min-width: 38px;
  text-align: right;
  font-size: 12.5px;
  color: #e8eaed;
  font-variant-numeric: tabular-nums;
}
.gstate {
  flex: 1;
  min-width: 0;
  text-align: right;
  font-size: 12px;
  color: rgba(255, 255, 255, 0.6);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.row.col {
  flex-wrap: wrap;
  row-gap: 8px;
  padding: 9px 12px;
}
.chips {
  flex: 1;
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  min-width: 0;
}
.chip {
  flex: none;
  padding: 4px 10px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 8px;
  background: rgba(255, 255, 255, 0.06);
  color: #e8eaed;
  font-size: 12px;
  cursor: pointer;
  transition: background 0.12s ease, border-color 0.12s ease;
}
.chip:hover {
  background: var(--accent-faint);
  border-color: var(--accent-strong);
}
.chip.on {
  background: var(--accent-faint);
  border-color: var(--accent);
  color: #fff;
}
.chip:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

/* FPS 悬浮窗开关（与显示面板 HDR 开关同款） */
.toggle {
  position: relative;
  flex: none;
  width: 40px;
  height: 22px;
  border-radius: 99px;
  border: 1px solid rgba(255, 255, 255, 0.18);
  background: rgba(255, 255, 255, 0.1);
  cursor: pointer;
  transition: background 0.15s ease, border-color 0.15s ease;
}
.toggle .knob {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: #cfd2d6;
  transition: transform 0.15s ease, background 0.15s ease;
}
.toggle.on {
  background: var(--accent);
  border-color: var(--accent-hover);
  box-shadow: 0 0 8px var(--accent-strong);
}
.toggle.on .knob {
  transform: translateX(18px);
  background: #fff;
}
.toggle:disabled {
  opacity: 0.35;
  cursor: not-allowed;
}
.note {
  margin-top: 10px;
  font-size: 10.5px;
  line-height: 1.6;
  color: rgba(255, 255, 255, 0.3);
}

/* 头部附加按钮（slot 内容，需本地样式） */
.op {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: rgba(255, 255, 255, 0.6);
  cursor: pointer;
}
.op:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #fff;
}
.op.on {
  color: var(--accent-text);
}
.op[data-tip]::after {
  content: attr(data-tip);
  position: absolute;
  top: calc(100% + 6px);
  right: 0;
  padding: 3px 9px;
  border-radius: 7px;
  background: rgba(15, 16, 18, 0.96);
  border: 1px solid rgba(255, 255, 255, 0.1);
  color: #e8eaed;
  font-size: 12px;
  white-space: nowrap;
  opacity: 0;
  pointer-events: none;
  transition: opacity 0.15s ease 0.3s;
  z-index: 10;
}
.op:hover::after {
  opacity: 1;
}
</style>
