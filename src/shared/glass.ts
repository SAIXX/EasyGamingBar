import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * 给「当前窗口」开毛玻璃，并在 <html> 上标记 glass / no-glass 类；
 * base.css 的 --panel-bg 等变量据此在「半透明玻璃」与「纯色」间切换。
 * 统一 Acrylic 优先（真·背后内容模糊），Mica 降级。
 */
export async function applySelfGlass(): Promise<void> {
  const root = document.documentElement;
  try {
    await invoke("set_glass", { label: getCurrentWindow().label, enabled: true });
    root.classList.add("glass");
  } catch {
    root.classList.add("no-glass");
  }
}

/**
 * 临时撤掉当前窗口的毛玻璃（悬浮条折叠时用）：窗口内容移出后，系统背景板
 * 会在屏幕上露出一条黑带，必须一并 clear。撤底色同时摘掉 glass 类，
 * 免得没有模糊时 UI 还保持半透明。
 */
export async function setSelfGlass(enabled: boolean): Promise<void> {
  if (!enabled) {
    document.documentElement.classList.remove("glass");
  }
  try {
    await invoke("set_glass", { label: getCurrentWindow().label, enabled });
  } catch {
    /* 无毛玻璃环境忽略 */
  }
}
