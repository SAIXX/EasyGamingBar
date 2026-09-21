<script setup lang="ts">
/**
 * 手柄友好的下拉选择器（替代原生 <select>）。
 *
 * 为什么不用原生：<select> 的弹出候选列表是**系统级窗口**，XInput 轮询只能只读，
 * 手柄方向键/A/B 根本进不去那个菜单——原生下拉永远没法用手柄选。
 * 这里自己画列表，展开 / 上下移动 / A 确认 / B 收起全在页面内，手柄导航
 * （shared/gamepad.ts）可以完整驱动。
 *
 * 列表**必须留在 .gpsel 内部**（不能 Teleport 到 body）：手柄空间导航按
 * [data-gp-zone] 分区找候选，Teleport 出去的列表项不再属于内容区那个分区，
 * 上下键就永远进不去列表。就地绝对定位（放不下时向上翻）即可。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { t } from "../shared/i18n";
import { requestGpFocus } from "../shared/gamepad";

interface Option {
  value: string;
  label: string;
}

const props = defineProps<{
  modelValue: string;
  options: Option[];
}>();

const emit = defineEmits<{ (e: "update:modelValue", value: string): void }>();

const open = ref(false);
/** 向上翻（下方放不下时） */
const up = ref(false);
/** 列表最大高度（px）：按可用空间收紧，避免被内容区滚动容器裁掉 */
const maxH = ref(248);
const triggerEl = ref<HTMLElement | null>(null);
const listEl = ref<HTMLElement | null>(null);
const rootEl = ref<HTMLElement | null>(null);

/**
 * 手柄是否正在驱动本次交互（页面上有焦点环）。
 * 鼠标点选时不能把焦点环塞回触发按钮——pointerdown 已经把环摘了，
 * 再塞回去会在鼠标操作后凭空多出一个手柄焦点环。
 */
function gpDriven() {
  return !!document.querySelector(".gp-focus");
}

const currentLabel = computed(
  () =>
    props.options.find((o) => o.value === props.modelValue)?.label ??
    t("common.select")
);

/** 量一次可用空间：优先向下展开，下方不够就向上翻 */
function place() {
  const el = triggerEl.value;
  if (!el) return;
  const r = el.getBoundingClientRect();
  // 内容区（滚动容器）的可视范围
  const scroller = el.closest("main") as HTMLElement | null;
  const bounds = scroller
    ? scroller.getBoundingClientRect()
    : { top: 0, bottom: window.innerHeight };
  const roomBelow = bounds.bottom - r.bottom - 8;
  const roomAbove = r.top - bounds.top - 8;
  const need = Math.min(248, props.options.length * 30 + 8);
  if (roomBelow >= need || roomBelow >= roomAbove) {
    up.value = false;
    maxH.value = Math.max(120, Math.min(248, roomBelow));
  } else {
    up.value = true;
    maxH.value = Math.max(120, Math.min(248, roomAbove));
  }
}

function toggle() {
  if (open.value) {
    open.value = false;
    return;
  }
  place();
  open.value = true;
  // 手柄：展开瞬间把焦点锁进候选列表。否则焦点还停在触发按钮上，按「下」会
  // 按空间距离挑目标——列表第一项和下一个下拉框几乎等距，实测会直接跳过列表
  // 落到「输出 2」（用户反馈：「没有锁在选着界面，直接跳到下一个输出 2」）。
  if (gpDriven()) {
    void nextTick(() => {
      const el = (listEl.value?.querySelector<HTMLElement>(".gpsel-opt.on") ??
        listEl.value?.querySelector<HTMLElement>(".gpsel-opt")) ?? null;
      if (el) requestGpFocus(el);
    });
  }
}

function pick(value: string) {
  // 先记下是不是手柄在选：open 置 false 后列表 DOM 会被移除，焦点环也随节点一起没了
  const byGp = gpDriven();
  emit("update:modelValue", value);
  open.value = false;
  // 关键：选完必须把焦点还给触发按钮。
  // 列表是 v-if 渲染的，选项节点一移除，gamepad.ts 里的 focused 就指着已脱离
  // 文档的元素——下一次输入会被「元素不可见」自愈逻辑 clearFocus 掉，且
  // movedOnce 归零（A 键被防误触吞掉）。表现就是：选完一个设备后焦点环整个
  // 消失，想再换一个只能从头摇杆找回来，根本谈不上「自由切换」。
  // 还给触发按钮后按 A 即可再次展开，连续换设备。
  if (byGp) void nextTick(() => requestGpFocus(triggerEl.value));
}

function isActive(o: Option) {
  return o.value === props.modelValue;
}

/* 展开期间：手柄焦点离开「触发按钮 + 列表」就收起（覆盖手柄 B 键清焦点的情况）。
   只在展开时轮询，200ms 一次，开销可忽略。 */
let focusTimer: ReturnType<typeof setInterval> | null = null;
function stopFocusWatch() {
  if (focusTimer) {
    clearInterval(focusTimer);
    focusTimer = null;
  }
}
watch(open, (v) => {
  if (v) {
    // 打开时手柄在操作（有焦点环）→ 启用「焦点离开即收起」；鼠标打开的不启用
    const byGp = gpDriven();
    let miss = 0;
    focusTimer = setInterval(() => {
      const f = document.querySelector<HTMLElement>(".gp-focus");
      if (!f) {
        // 手柄直接清掉焦点环（不是移到别处）：连续两次探测不到就收起
        if (byGp && ++miss >= 2) open.value = false;
        return;
      }
      miss = 0;
      if (triggerEl.value?.contains(f)) return;
      if (f.closest(".gpsel-list")) return;
      open.value = false; // 焦点跑到别的地方去了
    }, 200);
  } else {
    stopFocusWatch();
    // 兜底：列表被收起时，若焦点环还挂在本列表的某个选项上（节点已移除或即将
    // 移除），把它接回触发按钮。否则手柄焦点会被 gamepad.ts 的自愈逻辑清掉。
    void nextTick(() => {
      const f = document.querySelector<HTMLElement>(".gp-focus");
      if (f && (!f.isConnected || f.closest(".gpsel-list"))) {
        requestGpFocus(triggerEl.value);
      }
    });
  }
});

/* 手柄在列表里按 B：gamepad.ts 会往列表上派发 gp-back（冒泡到根），
   这里负责收起列表——它那边只管把焦点环放回触发按钮，两边职责分开。 */
function onGpBack() {
  open.value = false;
}
onMounted(() => rootEl.value?.addEventListener("gp-back", onGpBack));

function onDocPointerDown(e: PointerEvent) {
  if (!open.value) return;
  const el = e.target as HTMLElement | null;
  if (el?.closest?.(".gpsel")) return;
  open.value = false;
}
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape" && open.value) open.value = false;
}
function onViewportChange() {
  if (open.value) place();
}

document.addEventListener("pointerdown", onDocPointerDown);
document.addEventListener("keydown", onKey);
window.addEventListener("resize", onViewportChange);
window.addEventListener("scroll", onViewportChange, true);
onBeforeUnmount(() => {
  rootEl.value?.removeEventListener("gp-back", onGpBack);
  document.removeEventListener("pointerdown", onDocPointerDown);
  document.removeEventListener("keydown", onKey);
  window.removeEventListener("resize", onViewportChange);
  window.removeEventListener("scroll", onViewportChange, true);
  stopFocusWatch();
});
</script>

<template>
  <div ref="rootEl" class="gpsel">
    <button
      ref="triggerEl"
      type="button"
      class="gpsel-trigger"
      :class="{ open }"
      data-gp-list-trigger
      @click="toggle"
    >
      <span class="gpsel-text" :class="{ ph: !modelValue }">{{ currentLabel }}</span>
      <svg
        class="gpsel-caret"
        width="10"
        height="10"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2.4"
        stroke-linecap="round"
        stroke-linejoin="round"
      >
        <polyline points="6 9 12 15 18 9"></polyline>
      </svg>
    </button>

    <!-- data-gp-list：告诉 shared/gamepad.ts「这是个浮层列表」，上下改走逐项环绕，
         不再按空间距离挑目标（否则列表项会和它盖住的下一个下拉框抢焦点） -->
    <div
      v-if="open"
      ref="listEl"
      class="gpsel-list"
      :class="{ up }"
      :style="{ maxHeight: maxH + 'px' }"
      data-gp-list
    >
      <button
        v-for="o in options"
        :key="o.value || '__none__'"
        type="button"
        class="gpsel-opt"
        :class="{ on: isActive(o) }"
        @click="pick(o.value)"
      >
        <span class="gpsel-opt-text">{{ o.label }}</span>
        <span v-if="isActive(o)" class="gpsel-check">✓</span>
      </button>
    </div>
  </div>
</template>

<style scoped>
.gpsel {
  position: relative;
  flex: 1;
  min-width: 0;
}

/* 触发按钮：与原来的原生 select 视觉一致（32px 高、8px 圆角、同边框） */
.gpsel-trigger {
  width: 100%;
  height: 32px;
  padding: 0 8px 0 10px;
  display: flex;
  align-items: center;
  gap: 8px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(255, 255, 255, 0.05);
  color: #e8eaed;
  font-size: 12.5px;
  font-family: inherit;
  text-align: left;
  cursor: pointer;
  transition: border-color 0.15s ease, background 0.15s ease;
}
.gpsel-trigger:hover,
.gpsel-trigger.open {
  border-color: var(--accent-strong, rgba(90, 140, 255, 0.6));
  background: rgba(255, 255, 255, 0.08);
}
.gpsel-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.gpsel-text.ph {
  color: rgba(255, 255, 255, 0.4);
}
.gpsel-caret {
  flex: none;
  color: rgba(255, 255, 255, 0.5);
  transition: transform 0.18s ease;
}
.gpsel-trigger.open .gpsel-caret {
  transform: rotate(180deg);
}

/* 候选列表（就地绝对定位，留在 [data-gp-zone] 内 → 手柄能走进去） */
.gpsel-list {
  position: absolute;
  left: 0;
  right: 0;
  top: calc(100% + 4px);
  z-index: 30;
  padding: 4px 0;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: #1b1e25;
  box-shadow: 0 14px 38px rgba(0, 0, 0, 0.55);
  overflow-y: auto;
  overscroll-behavior: contain;
}
.gpsel-list.up {
  top: auto;
  bottom: calc(100% + 4px);
}
</style>

<style>
/* 列表项被手柄聚焦时的高亮：.gp-focus 由 gamepad.ts 添加、元素属于本组件，
   用非 scoped 样式保证能选中（且限定 .gpsel-* 前缀，不污染其它界面） */
.gpsel-opt {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 7px 10px 7px 12px;
  border: none;
  background: transparent;
  color: #d7dae0;
  font-size: 12.5px;
  font-family: inherit;
  text-align: left;
  cursor: pointer;
}
.gpsel-opt:hover {
  background: rgba(255, 255, 255, 0.1);
}
.gpsel-opt.gp-focus {
  background: rgba(255, 255, 255, 0.16);
  color: #fff;
}
.gpsel-opt.on {
  color: var(--accent-text, #cfe0ff);
}
.gpsel-opt-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.gpsel-check {
  flex: none;
  color: var(--accent, #4a72e8);
  font-weight: 700;
}
</style>
