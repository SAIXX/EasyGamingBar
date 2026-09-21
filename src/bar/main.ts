import { createApp } from "vue";
import App from "./App.vue";
import "../shared/base.css";
import { initLocale } from "../shared/i18n";
import { logLine } from "../shared/api";
import { startAlive } from "../shared/alive";

logLine("bar 脚本开始执行");
try {
  localStorage.setItem("bar.bootAt", String(Date.now()));
} catch {
  /* 忽略 */
}
// 先取到语言再挂载：首帧就是正确语言，不会闪一下再切换
// initLocale 内部有超时兜底，绝不会把挂载拖死
void initLocale().finally(() => {
  createApp(App).mount("#app");
  logLine("bar 页面已挂载");
  void startAlive("bar");
});
