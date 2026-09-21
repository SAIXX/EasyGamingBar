import { convertFileSrc } from "@tauri-apps/api/core";
import { t } from "./i18n";

/**
 * 皮肤强调色：选中皮肤后写入各窗口 --accent* 变量，
 * 按钮 / 开关 / 滑杆等控件跟随皮肤换色（默认值见 shared/base.css）。
 */
export interface SkinAccent {
  /** 主强调色（主按钮、开关 on、滑杆） */
  main: string;
  /** 悬停提亮一档 */
  hover: string;
  /** 强调色底上的前景文字（亮色强调配深字保证对比） */
  contrast: string;
  /** 浅强调底上的文字（导航选中、键帽等） */
  text: string;
}

export interface SkinDef {
  id: string;
  /** 词条 key：皮肤名随界面语言走，用 skinName() 取值 */
  nameKey: string;
  /** 主图（public/skins 下）：纵向窗口 contain 完整展示，不裁标志 */
  url: string;
  /** 宽幅补边版（按图自身边缘色补边、标志居中）：悬浮条等窄条 cover 铺满 */
  wideUrl: string;
  /** contain 留白处的衬底（与图片边缘色一致，视觉上无缝） */
  base: string;
  /** 皮肤选择器缩略图底色 */
  thumb: string;
  accent?: SkinAccent;
}

/** 皮肤显示名（内置皮肤走词条，随界面语言） */
export function skinName(s: SkinDef): string {
  return t(s.nameKey);
}

/** settings.skin 的形态 */
export interface SkinConf {
  /** 内置皮肤 id；导入自定义图片时为 "custom" */
  id?: string;
  /** 自定义图片的本地绝对路径（app_data_dir/skins 内） */
  path?: string;
  /** 背景不透明度 8-95（%），默认 55 */
  opacity?: number;
}

export const BUILTIN_SKINS: SkinDef[] = [
  {
    id: "marathon",
    nameKey: "skin.marathon",
    url: "skins/marathon.png",
    wideUrl: "skins/marathon-wide.png",
    base: "#0a100a",
    thumb: "#101710",
    accent: { main: "#befa04", hover: "#d2ff3d", contrast: "#1a2402", text: "#dcf89b" },
  },
  {
    id: "cyberpunk",
    nameKey: "skin.cyberpunk",
    url: "skins/cyberpunk.png",
    wideUrl: "skins/cyberpunk-wide.png",
    base: "linear-gradient(#585610 0%, #585610 20%, #f8f244 80%, #fdf845 100%)",
    thumb: "#b0a81e",
    accent: { main: "#f2df10", hover: "#ffe93a", contrast: "#1e1a02", text: "#f1eba4" },
  },
  {
    id: "starfield",
    nameKey: "skin.starfield",
    url: "skins/starfield.png",
    wideUrl: "skins/starfield-wide.png",
    base: "linear-gradient(#2a495a, #1a1d1e)",
    thumb: "#223642",
    accent: { main: "#e64a50", hover: "#f25b62", contrast: "#ffffff", text: "#f5c9cb" },
  },
];

/** accentVars 写入的全部变量名（换回经典深色时逐一移除，回落 base.css 默认值） */
const ACCENT_KEYS = [
  "--accent",
  "--accent-hover",
  "--accent-contrast",
  "--accent-text",
  "--accent-faint",
  "--accent-mid",
  "--accent-strong",
];

/** 读取当前皮肤配置；未设置 / 「经典深色」时返回 null */
export function readSkin(settings: Record<string, unknown>): SkinConf | null {
  const s = settings["skin"] as SkinConf | undefined;
  if (!s || (!s.path && (!s.id || s.id === "none"))) return null;
  return s;
}

/** #rgb / #rrggbb → rgba() */
function rgba(hex: string, alpha: number): string {
  let v = hex.replace("#", "");
  if (v.length === 3) v = v.split("").map((c) => c + c).join("");
  const n = parseInt(v, 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${alpha})`;
}

/**
 * 当前皮肤的 --accent* CSS 变量组；经典深色 / 自定义图片皮肤返回空
 * （自定义图没有明确的主题色，保持默认蓝）。
 */
export function accentVars(settings: Record<string, unknown>): Record<string, string> {
  const s = readSkin(settings);
  if (!s || s.path) return {};
  const a = BUILTIN_SKINS.find((b) => b.id === s.id)?.accent;
  if (!a) return {};
  return {
    "--accent": a.main,
    "--accent-hover": a.hover,
    "--accent-contrast": a.contrast,
    "--accent-text": a.text,
    "--accent-faint": rgba(a.main, 0.14),
    "--accent-mid": rgba(a.main, 0.32),
    "--accent-strong": rgba(a.main, 0.6),
  };
}

/** 把强调色变量写到窗口根元素（供没有皮肤背景的窗口跟随换色，如组件面板） */
export function bindAccent(settings: Record<string, unknown>): void {
  const style = document.documentElement.style;
  for (const k of ACCENT_KEYS) style.removeProperty(k);
  for (const [k, v] of Object.entries(accentVars(settings))) style.setProperty(k, v);
}

/**
 * 皮肤背景样式：图片上叠一层暗化遮罩（透明化处理），保证浅色文字可读；
 * 自定义 PNG 的透明通道原样保留，半透明区域直接透出桌面。
 * boost：面板类窗口加大遮罩（信息密度高，需要更强对比）。
 * opts.wide：悬浮条等窄条窗口用宽幅补边版 + cover 铺满（标志完整居中不被裁）；
 * 默认（纵向窗口）用主图 contain 完整显示，留白处衬 base 底色。
 * 返回值同时带上 --accent* 变量，绑定了皮肤背景的根元素即完成强调色换装。
 */
export function skinStyle(
  settings: Record<string, unknown>,
  boost = 0,
  opts?: { wide?: boolean }
): Record<string, string> {
  const vars = accentVars(settings);
  const s = readSkin(settings);
  if (!s) return vars;
  const opacity = Math.min(95, Math.max(8, s.opacity ?? 55)) / 100;
  const dim = Math.min(0.86, Math.max(0, 1 - opacity + boost));
  const def = BUILTIN_SKINS.find((b) => b.id === s.id);
  const c = dim.toFixed(3);
  if (s.path) {
    const url = convertFileSrc(s.path);
    return {
      ...vars,
      background: `linear-gradient(rgba(13,15,22,${c}), rgba(13,15,22,${c})), url("${url}") center / cover no-repeat`,
    };
  }
  const url = opts?.wide ? def?.wideUrl : def?.url;
  if (!url) return vars;
  if (opts?.wide) {
    return {
      ...vars,
      background: `linear-gradient(rgba(13,15,22,${c}), rgba(13,15,22,${c})), url("${url}") center / cover no-repeat`,
    };
  }
  return {
    ...vars,
    background: `linear-gradient(rgba(13,15,22,${c}), rgba(13,15,22,${c})), url("${url}") center / contain no-repeat, ${def?.base ?? "transparent"}`,
  };
}
