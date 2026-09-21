import { createApp } from "vue";
import App from "./App.vue";
import "../shared/base.css";
import { initLocale } from "../shared/i18n";
import { logLine } from "../shared/api";
import { startAlive } from "../shared/alive";

void initLocale().finally(() => {
  createApp(App).mount("#app");
  logLine("quick-drawer 页面已挂载");
  void startAlive("quick-drawer");
});
