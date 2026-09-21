import vue from "@vitejs/plugin-vue";
import { defineConfig } from "vite";
import { resolve } from "path";

// https://vite.dev/config/
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      // cargo 编译产物会触发 EBUSY，交给 tauri CLI 自己监视；
      // hooks/fps_hook 等子项目的 target 也要排除（否则 DLL 变动会让 Vite 崩溃退出）
      ignored: [
        "**/src-tauri/target/**",
        "**/src-tauri/**/target/**",
        "**/fps_hook.dll",
      ],
    },
  },
  build: {
    rollupOptions: {
      input: {
        bar: resolve(__dirname, "index.html"),
        settings: resolve(__dirname, "settings.html"),
        widget: resolve(__dirname, "widget.html"),
        "flyout-settings": resolve(__dirname, "flyout-settings.html"),
        "flyout-games": resolve(__dirname, "flyout-games.html"),
        "fps-overlay": resolve(__dirname, "fps-overlay.html"),
        "live-toolbar": resolve(__dirname, "live-toolbar.html"),
        "voice-hud": resolve(__dirname, "voice-hud.html"),
        tooltip: resolve(__dirname, "tooltip.html"),
        "game-intro": resolve(__dirname, "game-intro.html"),
        "quick-drawer": resolve(__dirname, "quick-drawer.html"),
        "first-run": resolve(__dirname, "first-run.html"),
      },
    },
  },
});
