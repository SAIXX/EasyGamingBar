<script setup lang="ts">
// 攻略助手面板：一个大按钮。点击 → Rust guide_run（截屏 → 视觉大模型识别当前任务
// → 内置浏览器跳 B 站搜攻略，并自动进第一条视频 / 匹配分P）。
// 进度经 guide://status 广播：capturing / thinking / opening / done / error。
// 面板本身不碰浏览器窗口——跳转后手柄自动进浏览器控制态（live_pad 既有逻辑）。
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import WidgetShell from "../WidgetShell.vue";
import WIcon from "../../shared/WIcon.vue";
import { guideRun, loadConfig, openSettingsCenter } from "../../shared/api";
import { t } from "../../shared/i18n";

type Phase = "idle" | "capturing" | "thinking" | "opening" | "done" | "error";
const phase = ref<Phase>("idle");
const query = ref("");
const err = ref("");
const configured = ref(true);

const busy = computed(() =>
  ["capturing", "thinking", "opening"].includes(phase.value)
);

const statusText = computed(() => {
  switch (phase.value) {
    case "capturing":
      return t("guide.status.capture");
    case "thinking":
      return t("guide.status.think");
    case "opening":
      return t("guide.status.open", { q: query.value });
    case "done":
      return t("guide.status.done", { q: query.value });
    case "error":
      return err.value;
    default:
      return t("guide.status.idle");
  }
});

async function run() {
  if (busy.value) return;
  err.value = "";
  phase.value = "capturing";
  try {
    await guideRun();
  } catch (e) {
    phase.value = "error";
    err.value = String(e ?? t("common.opFail"));
  }
}

let unlisten: Array<() => void> = [];

onMounted(async () => {
  unlisten.push(
    await listen<{ phase: string; msg: string }>("guide://status", (e) => {
      const p = String(e.payload?.phase ?? "");
      const m = String(e.payload?.msg ?? "");
      if (m) query.value = m;
      if (["capturing", "thinking", "opening", "done"].includes(p)) {
        phase.value = p as Phase;
        if (p !== "error") err.value = "";
      }
    })
  );
  // 配置就绪提示：缺任何一项就在面板上给「去设置」入口
  try {
    const c = await loadConfig();
    const s = c.settings;
    configured.value =
      s["guideEnabled"] === true &&
      !!String(s["guideBase"] ?? "").trim() &&
      !!String(s["guideModel"] ?? "").trim() &&
      !!String(s["guideKey"] ?? "").trim();
  } catch {
    configured.value = false;
  }
});

onBeforeUnmount(() => unlisten.forEach((f) => f()));
</script>

<template>
  <WidgetShell :title="t('widget.guide.name')" icon="search">
    <div class="wrap">
      <button
        class="big"
        :class="{ busy, err: phase === 'error' }"
        :disabled="busy"
        @click="run"
      >
        <WIcon name="search" :size="26" />
        <span>{{ t("guide.run") }}</span>
      </button>
      <div class="status" :class="phase">{{ statusText }}</div>
      <div v-if="!configured" class="noconf">
        {{ t("guide.noConfig") }}
        <button class="link" @click="openSettingsCenter('general')">
          {{ t("guide.goSettings") }}
        </button>
      </div>
      <div class="tip">{{ t("guide.tip") }}</div>
    </div>
  </WidgetShell>
</template>

<style scoped>
.wrap {
  display: flex;
  flex-direction: column;
  gap: 12px;
  height: 100%;
}
.big {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 12px;
  padding: 26px 16px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  border-radius: 12px;
  background: linear-gradient(
    135deg,
    color-mix(in srgb, var(--accent, #4a72e8) 30%, transparent),
    color-mix(in srgb, var(--accent, #4a72e8) 10%, transparent)
  );
  color: #f2f3f5;
  font-size: 16px;
  font-weight: 600;
  cursor: pointer;
  transition:
    filter 0.15s ease,
    opacity 0.15s ease;
}
.big:hover:not(:disabled) {
  filter: brightness(1.15);
}
.big:active:not(:disabled) {
  filter: brightness(0.9);
}
.big.busy {
  opacity: 0.6;
  cursor: default;
}
.status {
  font-size: 12.5px;
  line-height: 1.6;
  color: rgba(255, 255, 255, 0.62);
  min-height: 20px;
  word-break: break-all;
}
.status.error {
  color: #ff8d8d;
}
.status.done {
  color: #7fe0a0;
}
.noconf {
  font-size: 12px;
  line-height: 1.7;
  color: #ffd28d;
}
.link {
  border: none;
  background: none;
  color: var(--accent-text, #8fb0ff);
  font-size: 12px;
  cursor: pointer;
  text-decoration: underline;
  padding: 0;
}
.tip {
  margin-top: auto;
  font-size: 11px;
  color: rgba(255, 255, 255, 0.35);
  line-height: 1.6;
}
</style>
