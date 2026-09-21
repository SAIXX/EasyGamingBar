<script setup lang="ts">
// 组件窗口宿主：根据 ?id= 渲染对应组件面板
import PerformancePanel from "./panels/PerformancePanel.vue";
import AudioPanel from "./panels/AudioPanel.vue";
import DisplayPanel from "./panels/DisplayPanel.vue";
import GuidePanel from "./panels/GuidePanel.vue";

const params = new URLSearchParams(window.location.search);
const widgetId = params.get("id") ?? "";
</script>

<template>
  <PerformancePanel v-if="widgetId === 'performance'" />
  <AudioPanel v-else-if="widgetId === 'audio'" />
  <DisplayPanel v-else-if="widgetId === 'display'" />
  <GuidePanel v-else-if="widgetId === 'guide'" />
  <div v-else class="unknown">未知组件：{{ widgetId }}</div>
</template>

<style scoped>
.unknown {
  padding: 24px;
  color: rgba(255, 255, 255, 0.5);
  font-size: 13px;
}
</style>
