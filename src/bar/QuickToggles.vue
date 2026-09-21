<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import {
  hdrProbe,
  listAudioDevices,
  loadConfig,
  setDefaultAudioDevice,
  setHdr,
  toggleHdrHotkey,
} from "../shared/api";
import { t } from "../shared/i18n";

/* 悬浮条左缘的两个快捷开关：HDR 与 音频 A/B。
 * 全部是纯文字小图标，默认白色，上下组合排列：
 *     HDR
 *      ← 中间隔一段
 *     A        ← 默认输出命中哪个，哪个字母点亮为主题色
 *     B
 * 点亮色统一走 .qitem.on 的 var(--accent)，与悬浮条主题色严格一致（跟随皮肤换色）。
 * 是否显示由设置中心「快捷开关」页的 settings.quickSwitches 控制。 */

const settings = ref<Record<string, unknown>>({});

/* ---------------- HDR ---------------- */

const hdrOn = ref(false);
const hdrBusy = ref(false);

async function refreshHdr() {
  if (document.hidden) return; // 悬浮条收起/隐藏时不打扰游戏
  try {
    // 轻量探测：不做亮度 WMI/DDC 与分辨率枚举（那会周期性顿挫游戏）
    const [ccd, dxgi] = await hdrProbe();
    const v = dxgi ?? ccd;
    // 状态读取可用时以真实状态为准；不可用（null）则保持上次乐观值
    if (v != null) hdrOn.value = v;
  } catch {
    /* HDR 状态读取失败不影响切换操作 */
  }
}

async function toggleHdr() {
  if (hdrBusy.value) return;
  hdrBusy.value = true;
  const target = !hdrOn.value;
  hdrOn.value = target; // 乐观更新：Win+Alt+B 是纯开关
  try {
    await toggleHdrHotkey();
    await new Promise((r) => setTimeout(r, 1200)); // HDR 硬件切换有延迟，稍候读回
    await refreshHdr();
  } catch {
    // 快捷键不可用（Game Bar 被禁等）→ 回退 DisplayConfig 直设
    try {
      await setHdr(target);
      await new Promise((r) => setTimeout(r, 800));
      await refreshHdr();
    } catch {
      hdrOn.value = !target; // 两次都失败则回滚
    }
  } finally {
    hdrBusy.value = false;
  }
}

/** 悬浮提示：明确告知当前是开还是关 */
const hdrTip = computed(() => (hdrOn.value ? t("qt.hdr.on") : t("qt.hdr.off")));

/* ---------------- 音频 A/B ---------------- */

const defOutId = ref<string | null>(null);
const favA = ref<string | null>(null);
const favB = ref<string | null>(null);
const nameA = ref("");
const nameB = ref("");

async function refreshAudio() {
  try {
    const fav = (settings.value["audioFavorites"] as { out?: string[] } | undefined)?.out;
    favA.value = fav?.[0] ?? null;
    favB.value = fav?.[1] ?? null;
    const list = await listAudioDevices("output");
    defOutId.value = list.find((d) => d.is_default)?.id ?? null;
    nameA.value = list.find((d) => d.id === favA.value)?.name ?? "";
    nameB.value = list.find((d) => d.id === favB.value)?.name ?? "";
  } catch {
    /* 忽略：音频设备读取失败不影响切换 */
  }
}

/** 当前默认输出落在 A 还是 B（都不匹配为 null） */
const activeAB = computed<"A" | "B" | null>(() => {
  if (defOutId.value && favA.value && defOutId.value === favA.value) return "A";
  if (defOutId.value && favB.value && defOutId.value === favB.value) return "B";
  return null;
});

/** 点字母直接切到该设备（点已生效的那个不动作） */
async function switchTo(letter: "A" | "B") {
  const id = letter === "A" ? favA.value : favB.value;
  if (!id || activeAB.value === letter) return;
  try {
    await setDefaultAudioDevice(id);
    await refreshAudio();
  } catch {
    /* 忽略：切换失败不影响 UI */
  }
}

/** 每个字母的悬浮提示：设备名 + 是否为当前默认（即「是否开启」） */
function letterTip(letter: "A" | "B"): string {
  const id = letter === "A" ? favA.value : favB.value;
  const name = letter === "A" ? nameA.value : nameB.value;
  if (!id) return t("qt.ab.unset", { letter });
  const on = activeAB.value === letter;
  return t("qt.ab.tip", {
    letter,
    name: name || t("qt.ab.unnamed"),
    state: on ? t("qt.ab.current") : t("qt.ab.switch"),
  });
}

/* ---------------- 显示开关（来自设置中心「快捷开关」） ---------------- */

const showHdr = computed(
  () => (settings.value["quickSwitches"] as { hdr?: boolean } | undefined)?.hdr !== false
);
const showAb = computed(
  () => (settings.value["quickSwitches"] as { ab?: boolean } | undefined)?.ab !== false
);

/* 紧凑模式：整体缩小以适配更矮的悬浮条 */
const compact = computed(() => settings.value["compactMode"] === true);

/* ---------------- 生命周期 ---------------- */

let unlisten: Array<() => void> = [];
let poll: ReturnType<typeof setInterval> | null = null;

async function load() {
  try {
    const cfg = await loadConfig();
    settings.value = cfg.settings ?? {};
  } catch {
    /* 忽略：配置读取失败时使用默认（两开关都显示） */
  }
}

onMounted(async () => {
  await load();
  if (showHdr.value) await refreshHdr();
  if (showAb.value) await refreshAudio();
  // 周期读回外部变更（用户手动 Win+Alt+B、系统切换默认音频设备等）
  poll = setInterval(() => {
    if (document.hidden) return; // 悬浮条收起/隐藏时不打扰游戏
    if (showHdr.value) refreshHdr();
    if (showAb.value) refreshAudio();
  }, 2500);
  unlisten.push(await listen("config://updated", load));
});

onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
  if (poll) clearInterval(poll);
});
</script>

<template>
  <div class="quick-toggles" :class="{ compact }">
    <!-- HDR：纯文字图标，白 → 点亮为主题色 -->
    <button
      v-if="showHdr"
      class="qitem hdr"
      :class="{ on: hdrOn, busy: hdrBusy }"
      data-nav
      :data-tip="hdrTip"
      @click="toggleHdr"
    >
      HDR
    </button>

    <!-- 音频 A/B：在 HDR 下面排成一行「A / B」，两个字母各自可点，命中项点亮为主题色 -->
    <div v-if="showAb" class="abrow">
      <button
        class="qitem ab"
        :class="{ on: activeAB === 'A', unset: !favA }"
        data-nav
        :data-tip="letterTip('A')"
        @click="switchTo('A')"
      >
        A
      </button>
      <span class="sep">/</span>
      <button
        class="qitem ab"
        :class="{ on: activeAB === 'B', unset: !favB }"
        data-nav
        :data-tip="letterTip('B')"
        @click="switchTo('B')"
      >
        B
      </button>
    </div>
  </div>
</template>

<style scoped>
/* 文字图标上下组合：HDR 一行在上，中间隔一段，A/B 一行在下 */
.quick-toggles {
  display: flex;
  flex-direction: column;
  align-items: stretch;
  gap: 5px; /* HDR 与 A/B 排之间隔一段 */
  flex: none;
}

/* 单个文字图标：纯字体，没有任何外框 / 底色 / 圆角包围；默认白色微透明 */
.qitem {
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: none;
  background: transparent;
  color: rgba(255, 255, 255, 0.72);
  font-weight: 700;
  line-height: 1;
  letter-spacing: 0.5px;
  cursor: pointer;
  transition: color 0.15s ease, opacity 0.15s ease,
    transform 0.18s cubic-bezier(0.34, 1.56, 0.64, 1), text-shadow 0.15s ease;
  will-change: transform;
}
/* 悬停：只提亮字体 + 极淡白色辉光，仍然不出现任何包围形状 */
.qitem:hover {
  color: #fff;
  text-shadow: 0 0 6px rgba(255, 255, 255, 0.35);
}
.qitem:active {
  transform: scale(0.9);
  transition-duration: 0.08s;
}
.qitem.busy {
  opacity: 0.55;
  pointer-events: none;
}

/* 点亮：HDR 与 A/B 共用完全同一条规则 ——
   只有字体本身变主题色，带一点透明 + 霓虹辉光（科技感）；无外圈、无底色、无描边 */
.qitem.on {
  color: var(--accent);
  opacity: 0.9;
  text-shadow: 0 0 7px var(--accent-strong), 0 0 2px var(--accent-mid);
}
.qitem.on:hover {
  opacity: 1;
  text-shadow: 0 0 11px var(--accent-strong), 0 0 3px var(--accent);
}

/* HDR：独占一行 */
.qitem.hdr {
  height: 22px;
  min-width: 28px;
  font-size: 9.5px;
  letter-spacing: 0.3px;
}

/* A / B：在 HDR 下面排成一排（A / B），两个字母各自可点 */
.abrow {
  display: flex;
  flex-direction: row;
  align-items: center;
  justify-content: center;
  gap: 1px;
  height: 15px;
}
.sep {
  font-size: 9px;
  font-weight: 400;
  line-height: 1;
  color: rgba(255, 255, 255, 0.35);
}
.qitem.ab {
  height: 15px;
  min-width: 15px;
  padding: 0 1px;
  font-size: 10px;
}
/* 未配置该路设备：淡化且不可点（Tooltip 提示去音频设置） */
.qitem.ab.unset {
  opacity: 0.35;
  cursor: default;
}
.qitem.ab.unset:hover {
  color: rgba(255, 255, 255, 0.72);
  text-shadow: none;
}

/* 紧凑模式整体缩一档，适配更矮的悬浮条 */
.quick-toggles.compact {
  gap: 4px;
}
.quick-toggles.compact .qitem.hdr {
  height: 20px;
  min-width: 26px;
  font-size: 9px;
}
.quick-toggles.compact .abrow {
  height: 13px;
  gap: 0.5px;
}
.quick-toggles.compact .qitem.ab {
  height: 13px;
  min-width: 13px;
  font-size: 9px;
}
.quick-toggles.compact .sep {
  font-size: 8px;
}
</style>
