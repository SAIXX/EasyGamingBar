<script setup lang="ts">
/**
 * 首启语言选择：仅在 config.settings.lang 未设置时由悬浮条弹出一次。
 * 选定后写入配置（Rust 广播 config://updated），所有窗口同步刷新，然后关窗。
 */
import { onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LANGS, currentLang, setLang, t, type Lang } from "../shared/i18n";
import { startGamepadNav } from "../shared/gamepad";

const picked = ref<Lang>(currentLang());
const busy = ref(false);

async function confirm() {
  if (busy.value) return;
  busy.value = true;
  try {
    await setLang(picked.value);
  } finally {
    await getCurrentWindow().close();
  }
}

onMounted(() => {
  // 手柄：A 确认 / B 返回；这里只做基本导航，选语言以鼠标点击为主
  startGamepadNav({ onBack: () => void getCurrentWindow().close() });
});
</script>

<template>
  <div class="wrap">
    <div class="brand">
      <span class="logo">EasyGamingBar</span>
    </div>

    <h1 class="title">{{ t("firstRun.title") }}</h1>
    <p class="sub">{{ t("firstRun.subtitle") }}</p>

    <div class="cards">
      <button
        v-for="l in LANGS"
        :key="l.id"
        class="card"
        :class="{ on: picked === l.id }"
        data-nav
        @click="picked = l.id"
      >
        <span class="native">{{ l.native }}</span>
        <span class="label">{{ l.label }}</span>
        <span class="check"><i /></span>
      </button>
    </div>

    <button class="go" :disabled="busy" @click="confirm">
      {{ t("firstRun.confirm") }}
    </button>
  </div>
</template>

<style scoped>
.wrap {
  width: 100vw;
  height: 100vh;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  padding: 28px;
  /* 透明无边框窗口：圆角由这里画，四角之外直接透出桌面 */
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  border-radius: 14px;
  box-shadow: var(--panel-inset);
}

.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 4px;
}
.logo {
  font-size: 15px;
  font-weight: 650;
  letter-spacing: 0.2px;
  color: var(--accent-text);
}

.title {
  font-size: 22px;
  font-weight: 650;
  color: #f2f4f7;
}

.sub {
  font-size: 12.5px;
  color: rgba(232, 234, 237, 0.62);
  text-align: center;
  line-height: 1.5;
}

.cards {
  display: flex;
  gap: 12px;
  margin-top: 8px;
}

.card {
  position: relative;
  width: 148px;
  padding: 16px 14px;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 4px;
  border: 1px solid var(--panel-border);
  border-radius: 12px;
  background: rgba(255, 255, 255, 0.04);
  color: #e8eaed;
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s, transform 0.15s;
}
.card:hover {
  background: rgba(255, 255, 255, 0.07);
}
.card.on {
  border-color: var(--accent);
  background: var(--accent-faint);
}

.native {
  font-size: 16px;
  font-weight: 620;
}
.label {
  font-size: 12px;
  color: rgba(232, 234, 237, 0.6);
}

.check {
  position: absolute;
  top: 12px;
  right: 12px;
  width: 14px;
  height: 14px;
  border: 1.5px solid rgba(255, 255, 255, 0.28);
  border-radius: 50%;
}
.card.on .check {
  border-color: var(--accent);
  background: var(--accent);
}
.card.on .check i {
  display: block;
  width: 100%;
  height: 100%;
  background: no-repeat center/9px
    url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='white' stroke-width='4' stroke-linecap='round' stroke-linejoin='round'><polyline points='20 6 9 17 4 12'/></svg>");
}

.go {
  margin-top: 10px;
  padding: 9px 26px;
  border: 0;
  border-radius: 10px;
  background: var(--accent);
  color: var(--accent-contrast);
  font-size: 13.5px;
  font-weight: 620;
  cursor: pointer;
}
.go:hover {
  background: var(--accent-hover);
}
.go:disabled {
  opacity: 0.6;
  cursor: default;
}
</style>
