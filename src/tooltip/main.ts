import { createApp } from "vue";
import App from "./App.vue";
import "../shared/base.css";
import { initLocale } from "../shared/i18n";
import { startAlive } from "../shared/alive";

void initLocale().finally(() => {
  createApp(App).mount("#app");
  void startAlive("tooltip");
});
