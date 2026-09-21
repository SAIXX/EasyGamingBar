<script setup lang="ts">
// 组件窗口通用外壳：深色面板 + 可拖动标题栏 + 图钉（置顶开关）+ 关闭
// 手柄两级导航：面板是「框选层」里的一个框（主题色描边包围整个面板）；
// 面板内 B = 返回框选层并框选整个主UI（面板保留不关），关闭走标题栏 × 按钮。
import { computed, onMounted, onBeforeUnmount, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { emit, listen } from "@tauri-apps/api/event";
import WIcon from "../shared/WIcon.vue";
import { applySelfGlass } from "../shared/glass";
import { enterOnShow, notifyReady } from "../shared/enter";
import { bindAccent } from "../shared/skins";
import { startGamepadNav } from "../shared/gamepad";
import { loadConfig, restoreFocus, wireClickFocus } from "../shared/api";
import { t } from "../shared/i18n";

defineProps<{ title: string; icon: string }>();

const rootEl = ref<HTMLElement | null>(null);
const pinned = ref(true);
let offAccent: (() => void) | null = null;
let offGamepad: (() => void) | null = null;
let offTier: (() => void) | null = null;

// 入场必须走 Vue 绑定：enterOnShow 手动 classList.add 的类会被 Vue 的
// class patching 抹掉（tierClass 变化时整条 className 重写）→ 面板卡在 opacity:0
const entered = ref(false);

// 当前组件 id（widget.html?id=xxx）→ 框选层描边按 label 匹配
const myId = new URLSearchParams(window.location.search).get("id") ?? "";
const myLabel = `widget-${myId}`;

/** 框选层级状态（悬浮条仲裁广播 gp://tier）→ 本面板的描边样式：
 *  frame + 焦点在本框 = 高亮包围；frame + 焦点在别框 = 弱化包围；
 *  element + 所有者是自己 = 已进入操作态（实线包围）；其余不描边。 */
const tier = ref<{ mode: string; owner: string | null; focus: string | null } | null>(null);
const tierClass = computed(() => {
  const s = tier.value;
  if (!s) return "";
  if (s.mode === "frame") return s.focus === myLabel ? "frame-focal" : "frame-dim";
  if (s.mode === "element" && s.owner === myLabel) return "frame-in";
  return "";
});

// 兜底：部分 WebView2 场景下 100vw/100vh 会按过期视口解析（内容缩成小块），
// 直接内联窗口视口尺寸，并在尺寸变化时同步
function fitRoot() {
  if (!rootEl.value) return;
  rootEl.value.style.width = window.innerWidth + "px";
  rootEl.value.style.height = window.innerHeight + "px";
}

function onWinResize() {
  fitRoot();
}

onMounted(() => {
  void applySelfGlass(); // 面板静止为主：Acrylic 优先
  fitRoot();
  window.addEventListener("resize", onWinResize);
  if (rootEl.value) enterOnShow(rootEl.value, () => (entered.value = true));
  notifyReady(); // 首帧就绪 → 悬浮条才 show 本窗口
  // 组件窗口也是 NOACTIVATE 覆盖窗：点击才取系统焦点，游戏里键盘才进得来
  wireClickFocus(`widget-${myId}`);
  // 组件窗口没有皮肤背景，控件强调色（.on 点亮、滑杆）仍跟随皮肤：
  // 把 --accent* 变量挂到根元素，设置里换肤后经 config://updated 同步
  void loadConfig().then((c) => bindAccent(c.settings));
  void listen("config://updated", async () => {
    bindAccent((await loadConfig()).settings);
  }).then((off) => {
    offAccent = off;
  });
  // 框选层描边跟随悬浮条的层级广播
  void listen<{ mode: string; owner: string | null; focus: string | null }>(
    "gp://tier",
    (e) => {
      tier.value = e.payload ?? null;
    }
  ).then((off) => {
    offTier = off;
  });
  // 手柄：B = 返回框选层（悬浮条仲裁 → 框选整个主UI，本面板保留不关）。
  // 面板内方向键交给默认空间导航（滑杆聚焦时左右调值，见 shared/gamepad.ts）。
  // backDirect：二级界面里 B 一下就解锁回框选层（可再移到别的二级界面），
  // 不要先在面板内部空摘一次焦点环——那会变成「按两下 B 才出得去」。
  offGamepad = startGamepadNav({
    backDirect: true,
    onBack: () => {
      void emit("gp://leave");
    },
  });
});

onBeforeUnmount(() => {
  window.removeEventListener("resize", onWinResize);
  offAccent?.();
  offGamepad?.();
  offTier?.();
});

async function togglePin() {
  pinned.value = !pinned.value;
  await getCurrentWindow().setAlwaysOnTop(pinned.value);
}

async function close() {
  // 先把手前台还给游戏再隐藏：面板可能持有焦点（用户点过它），直接 hide 的话
  // Windows 会把前台塞给 Z 序里的下一个窗口（跳桌面 / 任务视图）
  void restoreFocus().catch(() => {});
  // 隐藏复用而非销毁：重开瞬时（销毁重建要重新拉起整个 webview 页面）
  await getCurrentWindow().hide();
  // 告知仲裁者（悬浮条）：本面板已不可见，清理手柄所有者/框选焦点
  void emit("gp://closed", { label: myLabel });
}
</script>

<template>
  <div ref="rootEl" class="widget" :class="[tierClass, { enter: entered }]">
    <header data-tauri-drag-region>
      <span class="htitle" data-tauri-drag-region>
        <WIcon :name="icon" :size="16" />
        <span data-tauri-drag-region>{{ title }}</span>
      </span>
      <span class="hops">
        <slot name="ops" />
        <button
          class="op"
          :class="{ on: pinned }"
          :data-tip="t('shell.pin')"
          @click="togglePin"
        >
          <WIcon name="pin" :size="14" />
        </button>
        <button class="op" :data-tip="t('shell.close')" @click="close">
          <WIcon name="close" :size="15" />
        </button>
      </span>
    </header>
    <div class="body">
      <slot />
    </div>
  </div>
</template>

<style scoped>
.widget {
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
  transform: translateY(10px) scale(0.95);
  transition: opacity 0.22s ease, transform 0.32s cubic-bezier(0.3, 1.35, 0.4, 1);
}
.widget.enter {
  opacity: 1;
  transform: none;
}
/* 手柄框选层描边（与 bar 同一套规则）：整个面板被主题色包围。
   outline 负偏移画在窗口内容区内，圆角略大于面板圆角 */
.widget.frame-focal {
  outline: 2.5px solid var(--accent, #4a72e8);
  outline-offset: -3px;
  box-shadow: var(--panel-inset), 0 10px 40px rgba(0, 0, 0, 0.5),
    0 0 16px var(--accent-strong, rgba(90, 140, 255, 0.55));
}
.widget.frame-dim {
  outline: 2px solid color-mix(in srgb, var(--accent, #4a72e8) 45%, transparent);
  outline-offset: -3px;
}
/* 元素层操作中：实线包围提示「当前只能操作这个面板」 */
.widget.frame-in {
  outline: 2px solid color-mix(in srgb, var(--accent, #4a72e8) 80%, transparent);
  outline-offset: -3px;
}

header {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: space-between;
  align-self: stretch;
  width: 100%;
  height: 42px;
  padding: 0 8px 0 14px;
  cursor: grab;
}
header:active {
  cursor: grabbing;
}

.htitle {
  display: inline-flex;
  align-items: center;
  gap: 9px;
  font-size: 13.5px;
  font-weight: 600;
  color: #f2f3f5;
}

.hops {
  display: inline-flex;
  gap: 2px;
}
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

.body {
  flex: 1;
  overflow-y: auto;
  padding: 10px 14px 14px;
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
