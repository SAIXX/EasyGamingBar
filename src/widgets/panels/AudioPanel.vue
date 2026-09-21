<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import WidgetShell from "../WidgetShell.vue";
import WIcon from "../../shared/WIcon.vue";
import { listen } from "@tauri-apps/api/event";
import {
  extractExeIcon,
  getDeviceVolume,
  listAudioDevices,
  listAudioSessions,
  loadConfig,
  setDefaultAudioDevice,
  setDeviceVolume,
  setSessionVolume,
  type AudioDevice,
  type AudioKind,
  type AudioSession,
  updateConfig,
} from "../../shared/api";
import { convertFileSrc } from "@tauri-apps/api/core";
import { t } from "../../shared/i18n";

interface AB {
  a: AudioDevice | null;
  b: AudioDevice | null;
}

/** 当前默认设备（output/input 各一个） */
const defOut = ref<AudioDevice | null>(null);
const defIn = ref<AudioDevice | null>(null);
/** 常用 A/B 设备（设置中心·音频设置里配置） */
const abOut = ref<AB>({ a: null, b: null });
const abIn = ref<AB>({ a: null, b: null });

const outVol = ref(0);
const inVol = ref(0);
const outMuted = ref(false);
const inMuted = ref(false);

const outSwitching = ref(false);
const inSwitching = ref(false);
const errMsg = ref("");
let errTimer: ReturnType<typeof setTimeout> | null = null;

function fail(msg: unknown) {
  errMsg.value = String(msg ?? t("common.opFail"));
  if (errTimer) clearTimeout(errTimer);
  errTimer = setTimeout(() => (errMsg.value = ""), 5000);
}

function isDef(d: AudioDevice | null, kind: AudioKind) {
  if (!d) return false;
  const def = kind === "output" ? defOut.value : defIn.value;
  return def?.id === d.id;
}

async function favorites(kind: AudioKind): Promise<(string | null)[]> {
  try {
    const cfg = await loadConfig();
    const fav = cfg.settings["audioFavorites"] as
      | { out?: string[]; input?: string[] }
      | undefined;
    const ids = kind === "output" ? fav?.out : fav?.input;
    if (ids && ids.length) return [ids[0] ?? null, ids[1] ?? null];
  } catch {
    /* 走回退 */
  }
  const list = await listAudioDevices(kind);
  const def = list.find((d) => d.is_default) ?? list[0];
  const other = list.find((d) => d.id !== def?.id);
  return [def?.id ?? null, other?.id ?? null];
}

function pickAB(list: AudioDevice[], favIds: (string | null)[]): AB {
  const resolve = (id: string | null) =>
    id ? list.find((d) => d.id === id) ?? null : null;
  const a = resolve(favIds[0]);
  const b = resolve(favIds[1]);
  if (a && b) return { a, b };
  // 回退：默认设备 + 列表中第一个非默认设备
  const def = list.find((d) => d.is_default) ?? list[0] ?? null;
  const other = list.find((d) => d.id && d.id !== def?.id) ?? null;
  return { a: a ?? def, b: b ?? other };
}

async function refresh() {
  try {
    const [outs, ins] = await Promise.all([
      listAudioDevices("output"),
      listAudioDevices("input"),
    ]);
    defOut.value = outs.find((d) => d.is_default) ?? null;
    defIn.value = ins.find((d) => d.is_default) ?? null;
    abOut.value = pickAB(outs, await favorites("output"));
    abIn.value = pickAB(ins, await favorites("input"));

    if (defOut.value) {
      const v = await getDeviceVolume(defOut.value.id, "output");
      outVol.value = v.volume;
      outMuted.value = v.muted;
    }
    if (defIn.value) {
      const v = await getDeviceVolume(defIn.value.id, "input");
      inVol.value = v.volume;
      inMuted.value = v.muted;
    }
  } catch (e) {
    console.error("音频设备刷新失败", e);
  }
}

/** 点 ⇄ 或点另一张卡片：一键切换默认设备 A ↔ B */
async function switchTo(kind: AudioKind, target: AudioDevice | null) {
  if (!target) return;
  const cur = kind === "output" ? defOut.value : defIn.value;
  if (cur?.id === target.id) return;
  if (kind === "output") outSwitching.value = true;
  else inSwitching.value = true;
  try {
    await setDefaultAudioDevice(target.id);
    await refresh();
  } catch (e) {
    fail(e);
  } finally {
    outSwitching.value = false;
    inSwitching.value = false;
  }
}

/* 滑块拖动 → 设置当前默认设备音量（60ms 防抖） */
let outTimer: ReturnType<typeof setTimeout> | null = null;
let inTimer: ReturnType<typeof setTimeout> | null = null;

function onOutSlider(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value);
  outVol.value = v;
  outMuted.value = false;
  if (!defOut.value) return;
  if (outTimer) clearTimeout(outTimer);
  outTimer = setTimeout(
    () => setDeviceVolume(defOut.value!.id, "output", { volume: v }).catch(() => {}),
    60
  );
}
function onInSlider(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value);
  inVol.value = v;
  inMuted.value = false;
  if (!defIn.value) return;
  if (inTimer) clearTimeout(inTimer);
  inTimer = setTimeout(
    () => setDeviceVolume(defIn.value!.id, "input", { volume: v }).catch(() => {}),
    60
  );
}

/** 开/关 = 默认输出静音切换；打开/关闭麦克风 = 默认输入静音切换 */
async function toggleOutMute() {
  if (!defOut.value) return;
  outMuted.value = !outMuted.value;
  await setDeviceVolume(defOut.value.id, "output", { mute: outMuted.value }).catch(() => {});
}
async function toggleInMute() {
  if (!defIn.value) return;
  inMuted.value = !inMuted.value;
  await setDeviceVolume(defIn.value.id, "input", { mute: inMuted.value }).catch(() => {});
}

const outVolLabel = computed(() =>
  outMuted.value ? t("audio.muted") : String(outVol.value)
);
const inVolLabel = computed(() =>
  inMuted.value ? t("audio.muted") : String(inVol.value)
);

/* ---------------- 音量混合器：按应用会话调节音量 ---------------- */

const sessions = ref<AudioSession[]>([]);
/** 置顶（星标）的应用，按名持久化在配置 mixerPins；系统声音固定用 "system" */
const pins = ref<string[]>([]);
/** pid → 应用图标资源地址 */
const icons = ref<Record<number, string>>({});
const iconFailed = new Set<number>();

const sessionKey = (s: AudioSession) => (s.system ? "system" : s.name.toLowerCase());
const isPinned = (s: AudioSession) => pins.value.includes(sessionKey(s));

function rowRank(s: AudioSession) {
  if (isPinned(s)) return 0;
  return s.active ? 1 : 2;
}
/** 置顶最前，其后按活动状态，同类按名称 */
const mixerRows = computed(() =>
  [...sessions.value].sort(
    (a, b) => rowRank(a) - rowRank(b) || a.name.localeCompare(b.name)
  )
);

async function togglePin(s: AudioSession) {
  const k = sessionKey(s);
  const i = pins.value.indexOf(k);
  if (i >= 0) pins.value.splice(i, 1);
  else pins.value.unshift(k);
  try {
    // 只提交 mixerPins，别的配置原样保留（Rust 侧落盘后广播 config://updated）
    await updateConfig({ settings: { mixerPins: [...pins.value] } });
  } catch (e) {
    fail(e);
  }
}

/** 点击应用图标 = 切换该应用静音 */
async function toggleSessionMute(s: AudioSession) {
  const next = !s.muted;
  s.muted = next;
  try {
    await setSessionVolume(s.pid, { mute: next });
  } catch (e) {
    s.muted = !next;
    fail(e);
  }
}

/* 滑块拖动 → 设置该会话音量（拖动时解除静音），80ms 防抖 */
const sessionTimers = new Map<number, ReturnType<typeof setTimeout>>();

function onSessionSlider(s: AudioSession, ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value);
  s.volume = v;
  if (sessionTimers.has(s.pid)) clearTimeout(sessionTimers.get(s.pid));
  sessionTimers.set(
    s.pid,
    setTimeout(() => {
      sessionTimers.delete(s.pid);
      setSessionVolume(s.pid, { volume: v, ...(s.muted ? { mute: false } : {}) })
        .then(() => {
          if (s.muted) s.muted = false;
        })
        .catch(() => {});
    }, 80)
  );
}

async function ensureIcons(list: AudioSession[]) {
  for (const s of list) {
    if (s.system || !s.path || icons.value[s.pid] || iconFailed.has(s.pid)) continue;
    try {
      icons.value[s.pid] = convertFileSrc(await extractExeIcon(s.path));
    } catch {
      iconFailed.add(s.pid); // 提取失败用默认音量图标，不再重试
    }
  }
}

async function refreshSessions() {
  try {
    const list = await listAudioSessions();
    // 拖动中的行保留本地值，避免轮询回跳
    const pending = [...sessionTimers.keys()];
    sessions.value = list.map((item) => {
      if (!pending.includes(item.pid)) return item;
      const local = sessions.value.find((x) => x.pid === item.pid);
      return local ? { ...item, volume: local.volume } : item;
    });
    ensureIcons(list);
  } catch (e) {
    console.error("音频会话刷新失败", e);
  }
}

async function loadPins() {
  try {
    const cfg = await loadConfig();
    const p = cfg.settings["mixerPins"];
    if (Array.isArray(p)) pins.value = p.filter((x): x is string => typeof x === "string");
  } catch {
    /* 配置读取失败不影响混合器 */
  }
}

let unlisten: Array<() => void> = [];
let pollTimer: ReturnType<typeof setInterval> | null = null;
onMounted(async () => {
  await refresh();
  await loadPins();
  await refreshSessions();
  pollTimer = setInterval(refreshSessions, 3000);
  unlisten.push(await listen("config://updated", refresh));
});
onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
  if (pollTimer) clearInterval(pollTimer);
  sessionTimers.forEach((t) => clearTimeout(t));
  if (outTimer) clearTimeout(outTimer);
  if (inTimer) clearTimeout(inTimer);
  if (errTimer) clearTimeout(errTimer);
});
</script>

<template>
  <WidgetShell :title="t('audio.title')" icon="volume">
    <div class="btns">
      <button class="b" :class="{ on: !outMuted }" @click="toggleOutMute">
        <WIcon name="volume" :size="15" /> {{ outMuted ? t("common.off") : t("common.on") }}
      </button>
      <button class="b" @click="toggleInMute">
        <WIcon name="mic" :size="15" /> {{ inMuted ? t("audio.micOn") : t("audio.micOff") }}
      </button>
    </div>

    <div class="srow">
      <WIcon name="volume" :size="15" />
      <input
        class="pink"
        type="range"
        min="0"
        max="100"
        :value="outVol"
        :style="{ '--fill': outVol + '%' }"
        @input="onOutSlider"
      />
      <span class="num">{{ outVolLabel }}</span>
    </div>
    <div class="srow">
      <WIcon name="mic" :size="15" />
      <input
        class="pink"
        type="range"
        min="0"
        max="100"
        :value="inVol"
        :style="{ '--fill': inVol + '%' }"
        @input="onInSlider"
      />
      <span class="num">{{ inVolLabel }}</span>
    </div>

    <div class="secname">{{ t("audio.defaultDevice") }}</div>

    <!-- 输出设备 A ⇄ B -->
    <div class="abrow">
      <div
        class="card"
        data-nav
        :class="{ active: isDef(abOut.a, 'output') }"
        :title="abOut.a?.name"
        @click="switchTo('output', abOut.a)"
      >
        <div class="tag"><WIcon name="volume" :size="12" /> {{ t("audio.outA") }}</div>
        <div class="name">{{ abOut.a?.name || t("common.unconfigured") }}</div>
        <div v-if="isDef(abOut.a, 'output')" class="cur">● {{ t("common.current") }}</div>
      </div>
      <div class="mid">
        <button
          class="switch"
          :disabled="outSwitching"
          :title="t('audio.switchOut')"
          @click="switchTo('output', isDef(abOut.a, 'output') ? abOut.b : abOut.a)"
        >
          <WIcon name="switch" :size="16" />
        </button>
      </div>
      <div
        class="card"
        data-nav
        :class="{ active: isDef(abOut.b, 'output') }"
        :title="abOut.b?.name"
        @click="switchTo('output', abOut.b)"
      >
        <div class="tag"><WIcon name="volume" :size="12" /> {{ t("audio.outB") }}</div>
        <div class="name">{{ abOut.b?.name || t("common.unconfigured") }}</div>
        <div v-if="isDef(abOut.b, 'output')" class="cur">● {{ t("common.current") }}</div>
      </div>
    </div>

    <!-- 输入设备 A ⇄ B -->
    <div class="abrow">
      <div
        class="card"
        data-nav
        :class="{ active: isDef(abIn.a, 'input') }"
        :title="abIn.a?.name"
        @click="switchTo('input', abIn.a)"
      >
        <div class="tag"><WIcon name="mic" :size="12" /> {{ t("audio.inA") }}</div>
        <div class="name">{{ abIn.a?.name || t("common.unconfigured") }}</div>
        <div v-if="isDef(abIn.a, 'input')" class="cur">● {{ t("common.current") }}</div>
      </div>
      <div class="mid">
        <button
          class="switch"
          :disabled="inSwitching"
          :title="t('audio.switchIn')"
          @click="switchTo('input', isDef(abIn.a, 'input') ? abIn.b : abIn.a)"
        >
          <WIcon name="switch" :size="16" />
        </button>
      </div>
      <div
        class="card"
        data-nav
        :class="{ active: isDef(abIn.b, 'input') }"
        :title="abIn.b?.name"
        @click="switchTo('input', abIn.b)"
      >
        <div class="tag"><WIcon name="mic" :size="12" /> {{ t("audio.inB") }}</div>
        <div class="name">{{ abIn.b?.name || t("common.unconfigured") }}</div>
        <div v-if="isDef(abIn.b, 'input')" class="cur">● {{ t("common.current") }}</div>
      </div>
    </div>

    <div class="secname">{{ t("audio.mixer") }}</div>
    <div v-if="!mixerRows.length" class="mixer-empty">{{ t("audio.mixerEmpty") }}</div>
    <div
      v-for="s in mixerRows"
      :key="s.pid"
      class="mrow"
      :class="{ idle: !s.active && !isPinned(s) }"
    >
      <div class="mhead">
        <button
          class="micon"
          :class="{ muted: s.muted }"
          :title="s.muted ? t('audio.unmute') : t('audio.mute')"
          @click="toggleSessionMute(s)"
        >
          <img v-if="icons[s.pid]" :src="icons[s.pid]" alt="" draggable="false" />
          <WIcon v-else name="volume" :size="19" />
          <span class="badge" :class="{ off: s.muted }">
            <WIcon name="volume" :size="9" />
          </span>
        </button>
        <div class="mname" :title="s.path || s.name">{{ s.name }}</div>
        <button
          class="star"
          :class="{ on: isPinned(s) }"
          :title="isPinned(s) ? t('audio.unpin') : t('audio.pin')"
          @click="togglePin(s)"
        >
          <WIcon :name="isPinned(s) ? 'starFill' : 'star'" :size="16" />
        </button>
      </div>
      <input
        class="pink"
        type="range"
        min="0"
        max="100"
        :value="s.volume"
        :style="{ '--fill': s.volume + '%' }"
        @input="onSessionSlider(s, $event)"
      />
    </div>

    <div v-if="errMsg" class="err">{{ errMsg }}</div>
    <div class="note">{{ t("audio.note") }}</div>
  </WidgetShell>
</template>

<style scoped>
.btns {
  display: grid;
  grid-template-columns: 1fr 1.4fr;
  gap: 8px;
  margin-bottom: 10px;
}
.b {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 7px;
  height: 32px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  background: rgba(255, 255, 255, 0.06);
  color: #e8eaed;
  font-size: 12.5px;
  cursor: pointer;
}
.b:hover {
  background: rgba(255, 255, 255, 0.1);
}
.b.on {
  background: #d7dae0;
  border-color: #d7dae0;
  color: #1b1d21;
}

.srow {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 5px 2px;
  color: #cfd2d6;
}
.srow .num {
  width: 30px;
  text-align: right;
  font-size: 12.5px;
  color: #e8eaed;
  font-variant-numeric: tabular-nums;
}

/* 滑块：跟随皮肤强调色（--accent*，默认经典蓝） */
input.pink {
  flex: 1;
  height: 18px;
  appearance: none;
  -webkit-appearance: none;
  background: transparent;
  cursor: pointer;
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

.secname {
  font-size: 13px;
  font-weight: 600;
  color: #f2f3f5;
  margin: 8px 2px 8px;
}

/* A/B 卡片 + 切换按钮 */
.abrow {
  display: flex;
  align-items: stretch;
  margin-bottom: 10px;
}
.card {
  flex: 1;
  min-width: 0;
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.04);
  padding: 8px 9px;
  cursor: pointer;
  transition: border-color 0.15s ease, background 0.15s ease;
}
.card:hover {
  border-color: rgba(255, 255, 255, 0.3);
}
.card.active {
  border-color: var(--accent);
  background: var(--accent-faint);
  box-shadow: 0 0 0 1px var(--accent-strong) inset;
}
.tag {
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 10.5px;
  color: rgba(255, 255, 255, 0.42);
  margin-bottom: 5px;
}
.tag b {
  color: var(--accent-text);
  font-weight: 600;
}
.card.active .tag b {
  color: #f2f3f5;
}
.name {
  font-size: 11.5px;
  line-height: 1.45;
  color: #d6d8db;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  min-height: 33px;
  word-break: break-all;
}
.card.active .name {
  color: #f2f3f5;
}
.cur {
  margin-top: 5px;
  font-size: 10px;
  color: var(--accent-text);
}
.mid {
  flex: none;
  display: flex;
  align-items: center;
  padding: 0 8px;
}
.switch {
  width: 34px;
  height: 34px;
  border-radius: 50%;
  border: 1.5px solid var(--accent);
  background: var(--accent-faint);
  color: var(--accent-hover);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  box-shadow: 0 0 10px var(--accent-strong);
  transition: transform 0.12s ease, background 0.15s ease;
}
.switch:hover {
  background: var(--accent-mid);
}
.switch:active {
  transform: scale(0.92);
}
.switch:disabled {
  opacity: 0.5;
}

/* 音量混合器：应用行（图标 + 名称 + 星标 + 滑块） */
.mixer-empty {
  padding: 4px 2px 6px;
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.35);
}
.mrow {
  margin-bottom: 12px;
}
.mrow.idle {
  opacity: 0.55;
}
.mhead {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 7px;
}
.micon {
  position: relative;
  flex: none;
  width: 42px;
  height: 42px;
  padding: 0;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 11px;
  background: #232529;
  display: flex;
  align-items: center;
  justify-content: center;
  color: #cfd2d6;
  cursor: pointer;
}
.micon:hover {
  border-color: rgba(255, 255, 255, 0.3);
}
.micon img {
  width: 25px;
  height: 25px;
  object-fit: contain;
  pointer-events: none;
}
.micon.muted img,
.micon.muted > svg {
  opacity: 0.45;
}
.badge {
  position: absolute;
  right: -5px;
  bottom: -5px;
  width: 17px;
  height: 17px;
  border-radius: 50%;
  background: #3a3d42;
  border: 1px solid rgba(255, 255, 255, 0.25);
  display: flex;
  align-items: center;
  justify-content: center;
  color: #e8eaed;
}
.badge.off {
  background: #8a2f36;
  border-color: #c0555d;
  color: #ffd7d7;
}
.mname {
  flex: 1;
  min-width: 0;
  font-size: 13px;
  color: #e8eaed;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.mrow.idle .mname {
  color: rgba(255, 255, 255, 0.6);
}
.star {
  flex: none;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: rgba(255, 255, 255, 0.38);
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
}
.star:hover {
  background: rgba(255, 255, 255, 0.08);
  color: rgba(255, 255, 255, 0.75);
}
.star.on {
  color: #ffce55;
}
.mrow .pink {
  width: 100%;
}

.err {
  margin-top: 8px;
  font-size: 11px;
  line-height: 1.55;
  color: #ff9c9c;
  word-break: break-all;
}

.note {
  margin-top: 10px;
  font-size: 10.5px;
  line-height: 1.6;
  color: rgba(255, 255, 255, 0.3);
}
</style>
