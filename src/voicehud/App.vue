<script setup lang="ts">
// 语音浮动 HUD：长按 B 语音输入时的状态提示（正在听 / 正在识别 / 已执行 / 没听清）。
// 独立顶层窗口（voice-hud）：锁定观看时直播工具条是隐藏的，HUD 必须仍然可见，
// 所以不放进工具条页面。窗口本体由 Rust 侧按需创建/显隐/定位（voice.rs），
// 这里只负责渲染 voice://state 五种状态。点击穿透 + 不抢焦点，绝不影响游戏。
import { onBeforeUnmount, onMounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import WIcon from "../shared/WIcon.vue";
import { t as tr } from "../shared/i18n";

interface VoiceState {
  state: string;
  text: string;
}

const vs = ref<VoiceState>({ state: "idle", text: "" });
let un: Array<() => void> = [];

onMounted(async () => {
  un.push(
    await listen<VoiceState>("voice://state", (e) => {
      const p = e.payload;
      if (p && typeof p.state === "string") vs.value = p;
    })
  );
});
onBeforeUnmount(() => un.forEach((f) => f()));
</script>

<template>
  <div v-show="vs.state !== 'idle'" class="hud">
    <template v-if="vs.state === 'listening'">
      <WIcon name="mic" :size="16" class="ico" />
      <span class="txt">{{ tr("live.voice.listen") }}</span>
      <span class="wave"><i v-for="n in 5" :key="n" :style="{ animationDelay: (n - 1) * 0.12 + 's' }" /></span>
    </template>
    <template v-else-if="vs.state === 'recognizing'">
      <span class="spin" />
      <span class="txt">{{ vs.text || tr("live.voice.rec") }}</span>
    </template>
    <template v-else-if="vs.state === 'success'">
      <span class="mark ok">✓</span>
      <span class="txt">{{ vs.text || tr("live.voice.ok") }}</span>
    </template>
    <template v-else-if="vs.state === 'error'">
      <span class="mark bad">✕</span>
      <span class="txt">{{ vs.text || tr("live.voice.err") }}</span>
    </template>
  </div>
</template>

<style scoped>
.hud {
  width: 100vw;
  height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  padding: 8px 14px;
  background: rgba(18, 20, 24, 0.72);
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 12px;
  color: #eef0f3;
  font-size: 13px;
  pointer-events: none;
  backdrop-filter: blur(6px);
}
.ico {
  color: #ff5d5d;
  flex: none;
}
.txt {
  white-space: nowrap;
}
/* 音量波形：五个小条错相跳动 */
.wave {
  display: inline-flex;
  align-items: flex-end;
  gap: 3px;
  height: 14px;
}
.wave i {
  width: 3px;
  height: 4px;
  border-radius: 2px;
  background: #ff6b6b;
  animation: egb-wave 0.7s ease-in-out infinite;
}
@keyframes egb-wave {
  0%,
  100% {
    height: 4px;
  }
  50% {
    height: 14px;
  }
}
/* 识别中：旋转圈 */
.spin {
  width: 14px;
  height: 14px;
  flex: none;
  border: 2px solid rgba(255, 255, 255, 0.25);
  border-top-color: #7fb0ff;
  border-radius: 50%;
  animation: egb-spin 0.8s linear infinite;
}
@keyframes egb-spin {
  to {
    transform: rotate(360deg);
  }
}
.mark {
  font-weight: 700;
  flex: none;
}
.mark.ok {
  color: #5ad07a;
}
.mark.bad {
  color: #ff6b6b;
}
</style>
