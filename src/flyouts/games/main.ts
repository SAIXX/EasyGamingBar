import { createApp } from "vue";
import App from "./App.vue";
import "../../shared/base.css";
import { initLocale } from "../../shared/i18n";

void initLocale().finally(() => createApp(App).mount("#app"));
