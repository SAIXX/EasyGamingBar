import { createApp } from "vue";
import WidgetHost from "./WidgetHost.vue";
import "../shared/base.css";
import { initLocale } from "../shared/i18n";
import { startAlive } from "../shared/alive";

void initLocale().finally(() => {
  createApp(WidgetHost).mount("#app");
  // 组件窗口 label 是 widget-<id>，从 URL 参数取回
  const id = new URLSearchParams(location.search).get("id") ?? "?";
  void startAlive(`widget-${id}`);
});
