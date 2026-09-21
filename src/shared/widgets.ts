import { t } from "./i18n";

export interface WidgetDef {
  id: string;
  /** 兼容字段：不再用于显示，统一走 widgetName()（ i18n ） */
  nameKey: string;
  descKey: string;
  width: number;
  height: number;
  ready: boolean;
}

/** 组件注册表：悬浮条中段图标、图钉面板与设置中心开关均由此驱动 */
export const WIDGETS: WidgetDef[] = [
  {
    id: "audio",
    nameKey: "widget.audio.name",
    descKey: "widget.audio.desc",
    width: 300,
    height: 640,
    ready: true,
  },
  {
    id: "performance",
    nameKey: "widget.performance.name",
    descKey: "widget.performance.desc",
    width: 320,
    height: 390,
    ready: true,
  },
  {
    id: "display",
    nameKey: "widget.display.name",
    descKey: "widget.display.desc",
    width: 300,
    height: 380,
    ready: true,
  },
  {
    id: "quick",
    nameKey: "widget.quick.name",
    descKey: "widget.quick.desc",
    width: 300,
    height: 430,
    ready: true,
  },
  {
    id: "guide",
    nameKey: "widget.guide.name",
    descKey: "widget.guide.desc",
    width: 300,
    height: 240,
    ready: true,
  },
];

export const widgetById = (id: string) => WIDGETS.find((w) => w.id === id);
export const widgetName = (w: WidgetDef) => t(w.nameKey);
export const widgetDesc = (w: WidgetDef) => t(w.descKey);
