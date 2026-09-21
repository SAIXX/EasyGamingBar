//! 界面多语言：语言选择存 config.settings.lang，切一次所有窗口同步刷新。
//!
//! 用法：
//! - 各窗口入口 `main.ts` 里 `await initLocale()` 再 `createApp(App).mount(...)`，首帧就是正确的语言
//! - 组件里直接用 `t("键")`（模板中同样可用）；lang 是 ref，`t()` 在渲染期间读它就具备响应式
//! - 切换语言统一调 `setLang()`：内部经 updateConfig 落盘并触发 config://updated，
//!   本模块订阅该事件刷新 lang，因此广播只能由 Rust 发（见 members 约定）

import { ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import { loadConfig, updateConfig } from "./api";
import { LOCALES, type Lang } from "./locales";

export type { Lang };

/** 语言候选（标签用各自母语，不翻译） */
export const LANGS: { id: Lang; label: string; native: string }[] = [
  { id: "zh", label: "Chinese", native: "简体中文" },
  { id: "en", label: "English", native: "English" },
];

const lang = ref<Lang>("zh");

/** 当前语言（ref，可直接用于模板判断） */
export const langRef = lang;
export const currentLang = (): Lang => lang.value;

function detect(): Lang {
  try {
    const nav = navigator.language || "zh";
    return nav.toLowerCase().startsWith("zh") ? "zh" : "en";
  } catch {
    return "zh";
  }
}

function apply(settings?: Record<string, unknown>) {
  const v = settings?.["lang"];
  lang.value = v === "zh" || v === "en" ? v : detect();
}

async function refresh() {
  try {
    // 加超时：取配置卡住（Rust 主线程忙 / 窗口层异常）时不能把页面挂载拖死，
    // 宁可先用系统语言渲染，等 config://updated 再纠正
    const cfg = await Promise.race([
      loadConfig(),
      new Promise<null>((r) => setTimeout(() => r(null), 1500)),
    ]);
    apply(cfg?.settings ?? undefined);
  } catch {
    apply();
  }
}

let unsub: (() => void) | null = null;

/** 每个窗口入口调用一次：读取语言并订阅后续变更 */
export async function initLocale() {
  await refresh();
  if (!unsub) unsub = await listen("config://updated", () => void refresh());
}

export function stopLocale() {
  unsub?.();
  unsub = null;
}

/** 切换语言：立即生效，并推送给所有窗口（写库 + Rust 广播） */
export async function setLang(next: Lang) {
  lang.value = next;
  try {
    await updateConfig({ settings: { lang: next } });
  } catch (e) {
    console.error("持久化语言失败", e);
  }
}

/** 取词条：{name} 形式的占位符用 vars 替换；缺词条回落中文，再缺就吐 key */
export function t(key: string, vars?: Record<string, string | number>): string {
  let s = LOCALES[lang.value]?.[key] ?? LOCALES.zh[key] ?? key;
  if (vars) {
    for (const [k, v] of Object.entries(vars)) {
      s = s.replaceAll(`{${k}}`, String(v));
    }
  }
  return s;
}
