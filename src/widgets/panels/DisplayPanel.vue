<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import WidgetShell from "../WidgetShell.vue";
import WIcon from "../../shared/WIcon.vue";
import {
  getDisplayStatus,
  setBrightness,
  setDisplayMode,
  setHdr,
  toggleHdrHotkey,
  type DisplayStatus,
} from "../../shared/api";
import { t } from "../../shared/i18n";

const status = ref<DisplayStatus | null>(null);
const busy = ref(false);
const err = ref("");
let errTimer: ReturnType<typeof setTimeout> | null = null;

function fail(msg: unknown) {
  err.value = String(msg ?? t("common.opFail"));
  if (errTimer) clearTimeout(errTimer);
  errTimer = setTimeout(() => (err.value = ""), 6000);
}

async function refresh() {
  try {
    status.value = await getDisplayStatus();
    const s = status.value;
    // 读不到当前值（软件调光模式）时从 100 起步 = 未干预的真实亮度
    bright.value = s.brightness ?? (s.brightness_supported ? 100 : 0);
    // 状态读取可用时以真实状态为准；不可用（null）则保持乐观值
    if (s.hdr_enabled != null) hdrOn.value = s.hdr_enabled;
  } catch (e) {
    fail(e);
  }
}

/* HDR：直接绑定 Win+Alt+B 系统快捷键（不做支持预判）；
   注入失败（Game Bar 被禁等）才回退 DisplayConfig 直设 */
const hdrOn = ref(false);

/* 亮度：滑栏拖动 → 防抖下发（内屏 WMI / 外接 DDC/CI，Twinkle Tray 同策略：
   读不到当前值也允许直接写，此时滑栏从 50% 起步） */
const bright = ref(50);
let brightTimer: ReturnType<typeof setTimeout> | null = null;

function onBrightSlider(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value);
  bright.value = v;
  if (!status.value?.brightness_supported) return;
  if (brightTimer) clearTimeout(brightTimer);
  brightTimer = setTimeout(() => setBrightness(v).catch(fail), 150);
}

/* 分辨率：全部可选项平铺，点击直接切换（保持当前刷新率，不支持时取其最高档） */
/* 下拉开合：互斥 + 点击外部关闭 */
const resOpen = ref(false);
const hzOpen = ref(false);

function toggleDd(which: "res" | "hz") {
  resOpen.value = which === "res" ? !resOpen.value : false;
  hzOpen.value = which === "hz" ? !hzOpen.value : false;
}

function onDocPointer(e: PointerEvent) {
  if ((e.target as HTMLElement)?.closest?.("[data-dd]")) return;
  resOpen.value = false;
  hzOpen.value = false;
}

const resItem = computed(() => {
  const s = status.value;
  if (!s) return null;
  return (
    s.resolutions.find(
      (r) => r.width === s.current_width && r.height === s.current_height
    ) ?? null
  );
});

/* 刷新率：当前分辨率支持的档位 */
const hzList = computed(() => resItem.value?.refreshes ?? []);

async function applyMode(width: number, height: number, hz: number) {
  busy.value = true;
  try {
    await setDisplayMode(width, height, hz);
    await refresh();
  } catch (e) {
    fail(e);
    await refresh();
  } finally {
    busy.value = false;
  }
}

async function pickRes(r: { width: number; height: number; refreshes: number[] }) {
  const s = status.value;
  if (!s || busy.value) return;
  // 尽量保持当前刷新率，新分辨率不支持时取其最高档
  const hz = r.refreshes.includes(s.current_refresh) ? s.current_refresh : r.refreshes[0];
  await applyMode(r.width, r.height, hz);
}

async function pickHz(hz: number) {
  const s = status.value;
  if (!s || busy.value) return;
  await applyMode(s.current_width, s.current_height, hz);
}

/* HDR：发送 Win+Alt+B（Xbox Game Bar HDR 开关）；
   注入失败时回退到 DisplayConfig 系统开关 */
async function toggleHdr() {
  if (busy.value) return;
  busy.value = true;
  const target = !hdrOn.value;
  hdrOn.value = target; // 乐观更新：Win+Alt+B 是纯开关
  try {
    await toggleHdrHotkey();
    // HDR 切换有硬件延迟，稍候读回真实状态（读不到则保持乐观值）
    await new Promise((r) => setTimeout(r, 1200));
    await refresh();
  } catch (e) {
    // 快捷键不可用 → 回退 DisplayConfig 直设
    try {
      await setHdr(target);
      await new Promise((r) => setTimeout(r, 800));
      await refresh();
    } catch (e2) {
      hdrOn.value = !target;
      fail(e2);
    }
  } finally {
    busy.value = false;
  }
}

onMounted(() => {
  document.addEventListener("pointerdown", onDocPointer);
  refresh();
});
onBeforeUnmount(() => {
  document.removeEventListener("pointerdown", onDocPointer);
  if (brightTimer) clearTimeout(brightTimer);
  if (errTimer) clearTimeout(errTimer);
});
</script>

<template>
  <WidgetShell :title="t('display.title')" icon="monitor">
    <template v-if="status">
      <!-- 亮度（滑栏） -->
      <div class="row">
        <span class="label"
          ><WIcon name="sun" :size="15" /> {{ t("display.brightness") }}</span
        >
        <input
          class="pink"
          type="range"
          min="0"
          max="100"
          :value="bright"
          :disabled="!status.brightness_supported"
          :style="{ '--fill': bright + '%' }"
          @input="onBrightSlider"
        />
        <span class="num">{{
          status.brightness_supported ? bright : t("display.unsupported")
        }}</span>
      </div>

      <!-- 分辨率（下拉列表，Windows 设置风格） -->
      <div class="row col">
        <span class="label"
          ><WIcon name="monitor" :size="15" /> {{ t("display.resolution") }}</span
        >
        <div class="dd" data-dd>
          <button
            class="dd-trigger"
            :disabled="busy"
            @click="toggleDd('res')"
          >
            <span class="dd-cur">{{
              resItem ? `${resItem.width} × ${resItem.height}` : "—"
            }}</span>
            <WIcon name="down" :size="12" />
          </button>
          <div v-if="resOpen" class="dd-list">
            <button
              v-for="r in status.resolutions"
              :key="r.width + 'x' + r.height"
              class="dd-item"
              :class="{ on: r.width === status.current_width && r.height === status.current_height }"
              @click="pickRes(r); resOpen = false"
            >
              <span class="dd-txt"
                >{{ r.width }} × {{ r.height
                }}<template v-if="r.width === status.current_width && r.height === status.current_height"
                  >{{ t("common.current2") }}</template
                ></span
              >
            </button>
          </div>
        </div>
      </div>

      <!-- 刷新率（当前分辨率的档位下拉） -->
      <div class="row col">
        <span class="label"
          ><WIcon name="monitorAct" :size="15" /> {{ t("display.refresh") }}</span
        >
        <div class="dd" data-dd>
          <button
            class="dd-trigger"
            :disabled="busy || !hzList.length"
            @click="toggleDd('hz')"
          >
            <span class="dd-cur">{{
              hzList.includes(status.current_refresh) ? `${status.current_refresh} Hz` : "—"
            }}</span>
            <WIcon name="down" :size="12" />
          </button>
          <div v-if="hzOpen" class="dd-list">
            <button
              v-for="hz in hzList"
              :key="hz"
              class="dd-item"
              :class="{ on: hz === status.current_refresh }"
              @click="pickHz(hz); hzOpen = false"
            >
              <span class="dd-txt"
                >{{ hz }} Hz<template v-if="hz === status.current_refresh"
                  >{{ t("common.current2") }}</template
                ></span
              >
            </button>
          </div>
        </div>
      </div>

      <!-- HDR（开关按钮，直接绑 Win+Alt+B） -->
      <div class="row">
        <span class="label"><WIcon name="sun" :size="15" /> HDR</span>
        <span class="hdrstate">{{ hdrOn ? t("common.enabled") : t("common.disabled") }}</span>
        <button
          class="toggle"
          :class="{ on: hdrOn }"
          :disabled="busy"
          :title="t('display.hdrSwitch')"
          @click="toggleHdr"
        >
          <span class="knob" />
        </button>
      </div>

      <div v-if="err" class="err">{{ err }}</div>
      <div v-else class="note">
        {{ t("display.notePrefix")
        }}<template v-if="!status.brightness_supported">{{
          t("display.noteNoBrightness")
        }}</template
        ><template v-else-if="status.brightness == null">{{
          t("display.noteSoftwareDim")
        }}</template>
      </div>
    </template>
    <div v-else class="note">{{ t("display.loading") }}</div>
  </WidgetShell>
</template>

<style scoped>
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
  width: 74px;
  font-size: 13px;
  color: #d6d8db;
}

/* 亮度滑栏（与音频面板同款，跟随皮肤强调色） */
input.pink {
  flex: 1;
  min-width: 0;
  height: 18px;
  appearance: none;
  -webkit-appearance: none;
  background: transparent;
  cursor: pointer;
}
input.pink:disabled {
  opacity: 0.35;
  cursor: not-allowed;
}
input.pink::-webkit-slider-runnable-track {
  height: 5px;
  border-radius: 3px;
  background: linear-gradient(
    to right,
    var(--accent) 0%,
    var(--accent) var(--fill, 50%),
    rgba(255, 255, 255, 0.14) var(--fill, 50%)
  );
}
input.pink::-webkit-slider-thumb {
  -webkit-appearance: none;
  width: 13px;
  height: 13px;
  margin-top: -4px;
  border-radius: 50%;
  background: var(--accent-hover);
  border: 2px solid var(--accent);
  box-shadow: 0 0 6px var(--accent-strong);
}
input.pink::-moz-range-track {
  height: 5px;
  border-radius: 3px;
  background: rgba(255, 255, 255, 0.14);
}
input.pink::-moz-range-progress {
  height: 5px;
  border-radius: 3px;
  background: var(--accent);
}
input.pink::-moz-range-thumb {
  width: 13px;
  height: 13px;
  border-radius: 50%;
  background: var(--accent-hover);
  border: 2px solid var(--accent);
}
.num {
  flex: none;
  min-width: 34px;
  text-align: right;
  font-size: 12.5px;
  color: #e8eaed;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

/* 分辨率 / 刷新率 参数胶囊（直接点选） */
.row.col {
  flex-wrap: wrap;
  row-gap: 8px;
  padding: 9px 12px;
}
.dd {
  position: relative;
  flex: 1;
  min-width: 0;
}
.dd-trigger {
  width: 100%;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 6px 10px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 8px;
  background: rgba(255, 255, 255, 0.06);
  color: #f2f3f5;
  font-size: 12.5px;
  font-variant-numeric: tabular-nums;
  cursor: pointer;
  transition: background 0.12s ease, border-color 0.12s ease;
}
.dd-trigger:hover:not(:disabled) {
  background: var(--accent-faint);
  border-color: var(--accent);
}
.dd-trigger:disabled {
  opacity: 0.4;
  cursor: wait;
}
.dd-list {
  position: absolute;
  top: calc(100% + 6px);
  left: 0;
  right: 0;
  max-height: 248px;
  overflow-y: auto;
  z-index: 30;
  padding: 4px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 10px;
  background: rgba(14, 16, 20, 0.98);
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.55);
}
.dd-item {
  width: 100%;
  display: flex;
  align-items: center;
  padding: 8px 10px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: #e8eaed;
  font-size: 12.5px;
  font-variant-numeric: tabular-nums;
  text-align: left;
  cursor: pointer;
}
.dd-item:hover {
  background: rgba(255, 255, 255, 0.09);
}
.dd-item.on {
  background: rgba(255, 255, 255, 0.07);
  box-shadow: inset 3px 0 0 var(--accent, #ff7ac6);
  color: #fff;
}
.dd-txt {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.none {
  font-size: 12px;
  color: rgba(255, 255, 255, 0.35);
}

/* HDR 开关 */
.hdrstate {
  flex: 1;
  text-align: right;
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.45);
}
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
.err {
  margin-top: 10px;
  font-size: 11px;
  line-height: 1.6;
  color: #ff9d9d;
}
</style>
