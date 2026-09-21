/**
 * Xbox 手柄 UI 导航（配合 src-tauri/src/gamepad.rs 的轮询广播）。
 *
 * 输入事件 gamepad://input：{ d: 方向位掩码(1上 2下 4左 8右，十字键+左摇杆合并),
 * a, b: 是否按住, menu, slot }。
 *
 * 规则：
 * - 只有「窗口可见且有系统焦点」的窗口处理输入（每个窗口都监听同一广播，靠门控分流）；
 * - 左摇杆/十字键 = 空间方向选择最近的可交互元素（button / input / select / [data-nav]）；
 *   焦点在滑杆（input[type=range]）上时，左右改为调整数值；
 * - **界面里已不再有原生 `<select>`**（音频设备下拉换成了 settings/GpSelect.vue）：
 *   原生 select 的弹出候选列表是系统级窗口，手柄按键进不去，所以那个方案
 *   永远没法用手柄选。自定义下拉是「触发按钮 + 列表项」两个普通 button，
 *   A 就够展开、方向键进列表、A 确认——完全走这里的普通元素导航。
 *   下面针对 select 的分支（选择态 / adjustSelect）保留作兜底，正常不再触发。
 * - B 分层返回：选择态 → 退出选择；**列表层（[data-gp-list] 浮层内）→ 收起列表
 *   并把焦点还给触发控件**；焦点在控件上 → 熄灭焦点环（回到空间导航，例如从
 *   下拉框退回左侧标签页）；无焦点 → 宿主 onBack（关面板 / 收起悬浮条 / 关设置
 *   中心）。调着选项一按 B 不再把整个界面关掉；
 * - A = 点击焦点元素（滑杆与下拉框除外，见上）；窗口拿到焦点 / 重新显示会
 *   重新上锁并落 [data-gp-initial] 初始焦点（如设置中心的当前标签页按钮），
 *   未声明时首次摇杆按「离视口中心最近」取焦点；
 * - 摇杆按住有「先延迟后连发」的重复，与主机 UI 手感一致；
 * - 鼠标一动（按下/滚轮/真实移动）立即清除手柄焦点环，交还控制权。
 * - **输入设备自动切换**：鼠标活动 → 广播 `input://mouse`，宿主（悬浮条）据此
 *   解除手柄锁定（清 owner / 退框选层）回到鼠标模式；手柄有输入 → 广播
 *   `input://gamepad`，宿主据此把输入归属锁回手柄。两条广播都是全局的，
 *   每个窗口收到 `input://mouse` 都会摘掉自己的焦点环（鼠标可能动在别的窗口上）。
 * - 宿主可用 [data-gp-zone] 把界面划成分区（如设置中心的 导航列 / 内容区）：
 *   上下只在同区内移动（在内容区按上下不会被左侧导航吸走焦点），左右优先同区、
 *   同区没有候选才跨区（在内容区左缘按左即可回到导航列）。
 * - 两级导航（框选层）：宿主（悬浮条）激活 frameActive 时，方向/A/B 改走 onFrame*
 *   ——在「主UI整体 / 各已开组件面板」之间框选，A 确认进入该界面（元素层），
 *   B 从面板返回时框选整个主UI，须再按 A 才能操作主UI。见 bar/App.vue 仲裁状态机。
 */

import { getCurrentWindow } from "@tauri-apps/api/window";
import { emit, listen } from "@tauri-apps/api/event";
import { logLine } from "./api";

/** 诊断日志（排查「手柄没反应」）：只记录会改变状态的动作，量很小 */
function gpDiag(what: string) {
  void logLine(`[gp:${getCurrentWindow().label}] ${what}`, "info");
}

const FOCUSABLE = [
  "button:not(:disabled)",
  "[data-nav]",
  "input:not(:disabled)",
  "select:not(:disabled)",
  '[role="button"]',
].join(",");

/**
 * 由界面主动请求把手柄焦点放到某个元素上（例如自定义下拉展开时把焦点锁进候选列表）。
 * 每个窗口是独立的 JS 上下文，这个模块级变量天然只作用于**本窗口**的导航实例。
 */
let navFocus: ((el: HTMLElement) => void) | null = null;
let navClear: (() => void) | null = null;

/**
 * 请求手柄焦点：传元素 = 把焦点环放到它上面；传 null = 熄灭本窗口的焦点环
 * （例如输入已被别的窗口接管，本窗口不该再留残环）。
 * 窗口没开手柄导航时静默忽略。
 */
export function requestGpFocus(el: HTMLElement | null) {
  if (el) navFocus?.(el);
  else navClear?.();
}

const FOCUS_CLASS = "gp-focus";
/** 下拉框「进入选择」态（A 进入后四向调选项、B/A 退出）的样式类 */
const SELECT_CLASS = "gp-select";
const DIR_NAMES = ["up", "down", "left", "right"] as const;
type Dir = (typeof DIR_NAMES)[number];

interface GamepadInput {
  d: number;
  a: boolean;
  b: boolean;
}

export interface GamepadNavOptions {
  /** B 键返回动作（关面板 / 收起悬浮条等） */
  onBack: () => void;
  /** 返回 false 时完全忽略手柄输入（如悬浮条折叠态，避免手柄误控折叠把手、干扰游戏，#3） */
  isEnabled?: () => boolean;
  /** 可选：接管某个方向（如组件窗口左右切换页面）。返回 true 表示已消费该方向，
   *  跳过默认空间选择，且不会连发（切换类动作为单次触发，按住不连续翻页）。 */
  onDir?: (dir: Dir) => boolean;
  /** 可选：手柄焦点变化通知（元素或 null）。手柄没有 hover，悬停类 UI（如游戏介绍卡）
   *  只能靠它驱动：落到某元素上显示、移开就收起。 */
  onFocus?: (el: HTMLElement | null) => void;
  /* ---- 框选层（两级导航）：主UI与各已开面板整体描边，先选框、A 确认进入 ----
   * frameActive() 为 true 时方向/A/B 全部改走 onFrame* 回调，元素层导航暂停。
   * 框选层的 A 不受「先移动才可确认」限制：B 返回框选后直接 A 进入是主路径。 */
  /** 框选层是否激活（宿主持有框选状态，如 bar 的仲裁状态机） */
  frameActive?: () => boolean;
  /** 框选层方向：在「主UI / 各面板」这些整体框之间移动选择 */
  onFrameDir?: (dir: Dir) => void;
  /** 框选层 A：进入当前框选的界面 */
  onFrameA?: () => void;
  /** 框选层 B：退出手柄框选（收起描边，回到无操作状态） */
  onFrameB?: () => void;
  /* ---- 输入设备自动切换（鼠标 ↔ 手柄）---- */
  /** 鼠标活动（按下 / 滚轮 / 真实移动）：宿主据此解除手柄锁定，切回鼠标模式 */
  onMouse?: () => void;
  /** 手柄有输入：宿主据此把输入归属锁到本窗口，切入手柄模式 */
  onGamepad?: () => void;
  /** B 直达 onBack，跳过「先熄灭焦点环」那一层（默认 false）。
   *  二级界面（组件面板）用：里面按 B 就该解锁本界面、回到框选层去选别的界面，
   *  先摘一次焦点环会变成「按两下 B 才出去」。下拉框选择态仍优先退出选择。 */
  backDirect?: boolean;
}

export function startGamepadNav(opts: GamepadNavOptions): () => void {
  let focused: HTMLElement | null = null;
  // 注册对外入口：界面可用 requestGpFocus() 把手柄焦点挪到指定元素 / 熄灭焦点环
  navFocus = focusEl;
  navClear = clearFocus;
  // 下拉框选择态：A 进入后上下（左右）逐项选值，A/B 退出——
  // 原生下拉弹窗是系统级菜单，手柄事件进不去，只能就地逐项切换
  let selectMode = false;
  let lastD = 0;
  let lastA = false;
  let lastB = false;
  // 防误触：拿到手柄焦点后必须先摇杆/十字键移动过一次，A 才生效——
  // 否则焦点环自动落在任意按钮上时，一颗 A 键就会触发意外动作（窜键）
  let movedOnce = false;
  let delayTimer: ReturnType<typeof setTimeout> | null = null;
  let repeatTimer: ReturnType<typeof setInterval> | null = null;
  let unlisten: (() => void) | null = null;
  // 最近一次聚焦的元素：B 取消聚焦后，下一次方向从原位置继续空间导航
  // （而不是跳回「离视口中心最近」——从下拉框 B 退出后按左应回到左侧导航列）
  let lastFocused: HTMLElement | null = null;

  /* ---- 长按确认（关机 / 强制关闭前台这类危险动作）----
   * 组件里这些按钮只认 pointerdown 起的「按住进度条」，A 的 click 被刻意忽略
   * （@click="holdable ? undefined : run"）。而手柄事件只在按键**状态变化**时到达，
   * 没有 pointer 语义——旧实现直接 focused.click()，于是纯手柄操作下「关机」和
   * 「强制关闭游戏」按 A 完全没有反应。
   * 协议：A 按下沿派发 gp-hold-start、松开（或焦点/窗口变化）派发 gp-hold-end，
   * 组件自己跑进度条并在满时执行。元素用 data-gp-hold 声明「我是长按类」。
   * 这与既有 gp-back 是同一套自定义事件约定（见 GpSelect.vue）。 */
  let holdEl: HTMLElement | null = null;
  let holdStartedAt = 0;

  /** 结束长按（幂等）：通知组件撤销进度条。 */
  function endHold() {
    if (!holdEl) return;
    const el = holdEl;
    holdEl = null;
    el.dispatchEvent(new CustomEvent("gp-hold-end", { bubbles: true }));
  }

  function clearFocus() {
    // 焦点被清掉（B 返回 / 窗口失焦 / 界面收起）时长按必须一起撤销：
    // 进度条若继续跑，用户在「松手丢事件」的情况下会误触发关机
    endHold();
    // 全局清理：HMR 重挂载可能产生多个导航实例，旧实例的焦点环要一并摘掉
    document.querySelectorAll(`.${FOCUS_CLASS},.${SELECT_CLASS}`).forEach((n) => {
      n.classList.remove(FOCUS_CLASS, SELECT_CLASS);
    });
    selectMode = false;
    if (focused) {
      const r = focused.getBoundingClientRect();
      if (r.width > 0 && r.height > 0) lastFocused = focused;
      focused.classList.remove(FOCUS_CLASS);
      focused = null;
      opts.onFocus?.(null);
    }
  }

  /** 进入/退出下拉框选择态（退出或焦点转移时摘掉样式） */
  function setSelectMode(on: boolean) {
    if (selectMode === on) return;
    selectMode = on;
    if (focused instanceof HTMLSelectElement) {
      focused.classList.toggle(SELECT_CLASS, on);
      gpDiag(`选择态 ${on ? "进入" : "退出"}（select，共 ${focused.options.length} 项）`);
    }
  }

  function focusEl(el: HTMLElement | null) {
    if (!el) return;
    // 全局清理焦点环：多实例（HMR）下各实例的 focused 会失联，
    // 只摘自己那颗会留下「两个都亮」的残影
    document.querySelectorAll("." + FOCUS_CLASS).forEach((n) => {
      if (n !== el) (n as HTMLElement).classList.remove(FOCUS_CLASS);
    });
    if (focused && focused !== el) focused.classList.remove(FOCUS_CLASS);
    const changed = focused !== el;
    if (changed) setSelectMode(false); // 焦点转移 = 退出选择态
    focused = el;
    focused.classList.add(FOCUS_CLASS);
    focused.scrollIntoView({ block: "nearest", inline: "nearest" });
    if (changed) {
      opts.onFocus?.(el);
      gpDiag(`焦点 → ${el.tagName}${el.dataset?.tip ? "(" + el.dataset.tip.slice(0, 12) + ")" : ""}`);
    }
  }

  function collect(): HTMLElement[] {
    return Array.from(document.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
      (el) => {
        if (el === focused) return true;
        // 显式跳过：格子内的删除 × 等鼠标专用危险小按钮，手柄不该聚焦到
        if (el.closest("[data-gp-skip]")) return false;
        const r = el.getBoundingClientRect();
        // 只排除「整体在视口上方/左方/右方」的元素（折叠后的悬浮条、翻页后备页等）。
        // 不排除下方元素：设置中心等长页面靠 scrollIntoView 边滚边选
        if (
          r.width <= 0 || r.height <= 0 ||
          r.bottom <= 0 || r.right <= 0 || r.left > innerWidth
        ) {
          return false;
        }
        return el.checkVisibility?.() ?? true;
      }
    );
  }

  /** 焦点在滑杆上时，左右改为调值（返回 true 表示已消费，不再移动焦点） */
  function adjustRange(dir: Dir): boolean {
    if (
      dir !== "left" && dir !== "right" ||
      !(focused instanceof HTMLInputElement) ||
      focused.type !== "range"
    ) {
      return false;
    }
    const el = focused;
    const min = Number(el.min || 0);
    const max = Number(el.max || 100);
    const span = max - min || 100;
    const step = Number(el.step) > 0 ? Number(el.step) : span / 20;
    const next = Math.min(
      max,
      Math.max(min, Number(el.value) + (dir === "right" ? step : -step))
    );
    el.value = String(next);
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return true;
  }

  /** 焦点在下拉框（select）上时，只有「选择态」（A 进入）才就地切换选项——
   *  原生下拉弹窗是系统级的，手柄事件进不去，绝不能让它弹出来。
   *  普通态一律返回 false 走空间移动：摇杆先移到下拉框，A 确认后才改值，
   *  避免「摇杆一碰设备就被切走」。进入选择态后四向切换并环绕。 */
  function adjustSelect(dir: Dir): boolean {
    if (!(focused instanceof HTMLSelectElement)) return false;
    if (!selectMode) return false;
    const el = focused;
    const len = el.options.length;
    if (len > 0) {
      const step = dir === "up" || dir === "left" ? -1 : 1;
      const next = (el.selectedIndex + step + len) % len;
      if (next !== el.selectedIndex) {
        el.selectedIndex = next;
        el.dispatchEvent(new Event("change", { bubbles: true }));
      }
    }
    return true; // 无可选项也消费方向：比弹原生菜单安全
  }

  /**
   * 列表层：焦点位于 [data-gp-list] 容器（自定义下拉的候选列表）内时，上下改为
   * 在列表项之间**环绕**移动，不再按空间距离挑目标。
   *
   * 为什么必须绕开空间导航：候选列表是绝对定位浮层，会盖在它后面的行上面——
   * 实测「输出 1」展开后，列表第 1 项中心与下一个下拉框「输出 2」的触发按钮
   * 中心只差 1px，纯粹靠 4px 阈值才没跳过去；首项按「上」也会直接弹回自己的
   * 触发按钮。浮层里的列表必须按「上一项 / 下一项」走，与视觉遮挡无关。
   *
   * 组件侧只需给列表容器加 data-gp-list、给触发控件加 data-gp-list-trigger。
   */
  function moveInList(dir: Dir): boolean {
    if (dir !== "up" && dir !== "down") return false;
    const list = focused?.closest<HTMLElement>("[data-gp-list]");
    if (!list) return false;
    const items = Array.from(list.querySelectorAll<HTMLElement>(FOCUSABLE));
    if (!items.length) return false;
    const idx = items.indexOf(focused as HTMLElement);
    if (idx >= 0) {
      const step = dir === "up" ? -1 : 1;
      focusEl(items[(idx + step + items.length) % items.length]);
    } else {
      // 焦点不在候选里（如浮层自身）：上→末项，下→首项
      focusEl(items[dir === "up" ? items.length - 1 : 0]);
    }
    return true;
  }

  /** 从 (cx,cy) 出发按 dir 做空间选择。分区规则（宿主用 data-gp-zone 划区，
   *  如设置中心的 导航列 / 内容区）：上下只在同区内移动（内容区里上下不被
   *  左侧导航列吸走焦点）；左右优先同区，同区没有候选才允许跨区（内容区左缘
   *  按左回到导航列）。excludeSelf 用于无焦点续航时排除「原位置的元素自己」。 */
  function pickFrom(
    items: HTMLElement[],
    cx: number,
    cy: number,
    dir: Dir,
    zone: Element | null,
    excludeSelf: HTMLElement | null
  ): HTMLElement | null {
    const pick = (allowCrossZone: boolean): HTMLElement | null => {
      let best: HTMLElement | null = null;
      let bestScore = Infinity;
      for (const el of items) {
        if (el === excludeSelf) continue;
        if (!allowCrossZone && el.closest("[data-gp-zone]") !== zone) continue;
        const b = el.getBoundingClientRect();
        const ex = b.left + b.width / 2 - cx;
        const ey = b.top + b.height / 2 - cy;
        let primary: number;
        let cross: number;
        if (dir === "left") {
          if (ex > -4) continue;
          primary = -ex;
          cross = Math.abs(ey);
        } else if (dir === "right") {
          if (ex < 4) continue;
          primary = ex;
          cross = Math.abs(ey);
        } else if (dir === "up") {
          if (ey > -4) continue;
          primary = -ey;
          cross = Math.abs(ex);
        } else {
          if (ey < 4) continue;
          primary = ey;
          cross = Math.abs(ex);
        }
        // 主轴距离 + 侧向偏移惩罚，偏向同排/同列元素
        const score = primary + cross * 2.2;
        if (score < bestScore) {
          bestScore = score;
          best = el;
        }
      }
      return best;
    };
    if (dir === "up" || dir === "down") return pick(false);
    return pick(false) ?? pick(true);
  }

  /** 离 (cx,cy) 最近的元素（无焦点进入时的兜底起点） */
  function nearestTo(items: HTMLElement[], cx: number, cy: number): HTMLElement | null {
    let best: HTMLElement | null = null;
    let bestScore = Infinity;
    for (const el of items) {
      const b = el.getBoundingClientRect();
      const dx = b.left + b.width / 2 - cx;
      const dy = b.top + b.height / 2 - cy;
      const s = dx * dx + dy * dy;
      if (s < bestScore) {
        bestScore = s;
        best = el;
      }
    }
    return best;
  }

  function moveOrAdjust(dir: Dir) {
    // 滑杆调值已在 onInput 优先处理（保证左/右在滑杆上仍调值、不被 onDir 翻页吞掉）
    const items = collect();
    if (!items.length) return;
    // 还没有焦点：从「上次焦点位置」按方向继续（B 取消聚焦后的原地续航——
    // 从下拉框 B 退出后按左应回到左侧导航列，而不是重新抓取原元素）。
    // 从未有焦点记录时以视口中心为起点，方向上没有候选就取最近的元素。
    if (!focused || !items.includes(focused)) {
      const origin =
        lastFocused && lastFocused.isConnected
          ? lastFocused.getBoundingClientRect()
          : null;
      const cx =
        origin && origin.width > 0 ? origin.left + origin.width / 2 : window.innerWidth / 2;
      const cy =
        origin && origin.height > 0
          ? origin.top + origin.height / 2
          : window.innerHeight / 2;
      const zone = origin ? lastFocused!.closest("[data-gp-zone]") : null;
      const exclude = origin ? lastFocused : null;
      const best = pickFrom(items, cx, cy, dir, zone, exclude);
      focusEl(best ?? nearestTo(items, cx, cy));
      return;
    }
    const r = focused.getBoundingClientRect();
    const best = pickFrom(
      items,
      r.left + r.width / 2,
      r.top + r.height / 2,
      dir,
      focused.closest("[data-gp-zone]"),
      focused
    );
    focusEl(best ?? focused);
  }

  function stopRepeat() {
    if (delayTimer) {
      clearTimeout(delayTimer);
      delayTimer = null;
    }
    if (repeatTimer) {
      clearInterval(repeatTimer);
      repeatTimer = null;
    }
  }

  /**
   * A/B「按下沿」看门狗：Rust 只在按键状态**变化**时发事件，所以按住不放期间
   * 不会有任何事件。若按住 A 时窗口被隐藏（Xbox 键切换显隐 / 关闭窗口），松开
   * 事件就丢了，lastA 会永远停在 true —— 之后无论怎么按 A 都会被 `!lastA` 挡掉，
   * 表现就是「A 键突然怎么按都没反应」（方向键不受影响，因为方向有连发计时器）。
   * 这里在静默 500ms 后把两个沿标记复位：按住不放时没有新事件，不会误触发。
   */
  let quietTimer: ReturnType<typeof setTimeout> | null = null;
  function armButtonWatchdog() {
    if (quietTimer) clearTimeout(quietTimer);
    quietTimer = setTimeout(() => {
      quietTimer = null;
      // 长按确认进行中：按住不放本来就不会有新事件，静默是**正常**的，
      // 不能按「静默 = 按键状态丢了」处理——那样 500ms 就把 1 秒的进度条掐断，
      // 表现为「按住 A 没反应」。只要还没超过长按时长上限（2.5s）就继续等。
      if (holdEl && performance.now() - holdStartedAt < 2500) {
        armButtonWatchdog();
        return;
      }
      // 静默即认为按键状态丢了：长按确认必须一并撤销，否则进度条会自己跑满
      // 把机器关掉（用户按住关机后按 Xbox 键隐藏界面，松开事件就收不到了）
      endHold();
      if (lastA || lastB) {
        lastA = false;
        lastB = false;
      }
    }, 500);
  }

  /** 门控：Rust 端已按焦点路由（无焦点时回退悬浮条），这里只确认页面活着 */
  function active(): boolean {
    return document.visibilityState === "visible";
  }

  let lastGpEmit = 0;
  /** 手柄一有输入即切回手柄模式：通知宿主把输入归属锁到本窗口 */
  function onGamepadActivity() {
    const now = performance.now();
    if (now - lastGpEmit < 200) return;
    lastGpEmit = now;
    opts.onGamepad?.();
    void emit("input://gamepad", { label: getCurrentWindow().label });
  }

  function onInput(p: GamepadInput) {
    if (!active()) return;
    if (p.d !== 0 || p.a || p.b) onGamepadActivity();
    // A 松开 → 结束「长按确认」。放在最前：下面的框选分支会提前 return，
    // 放后面会漏掉那里的松开事件。endHold 自身幂等，没在长按时是空操作。
    if (!p.a) endHold();
    // 焦点自愈：焦点元素已被隐藏（翻页/收起）→ 立即清焦点，宿主据此收卡片
    if (focused && focused.checkVisibility?.() === false) {
      clearFocus();
      movedOnce = false;
    }
    // 框选层优先于 isEnabled：框选激活时方向/A/B 全部走 onFrame*。
    // （isEnabled 若被宿主绑成「!frameActive」来暂停元素层，门控顺序颠倒会
    //  连框选分支一起拦掉——框选层就再也动不了了）
    if (opts.frameActive?.()) {
      if (p.d !== lastD) {
        const dir: Dir | null =
          p.d & 1 ? "up" : p.d & 2 ? "down" : p.d & 4 ? "left" : p.d & 8 ? "right" : null;
        stopRepeat();
        if (dir) {
          opts.onFrameDir?.(dir);
          delayTimer = setTimeout(() => {
            repeatTimer = setInterval(() => opts.onFrameDir?.(dir), 170);
          }, 420);
        }
      }
      if (p.a && !lastA) opts.onFrameA?.();
      if (p.b && !lastB) {
        stopRepeat();
        opts.onFrameB?.();
      }
      lastD = p.d;
      lastA = p.a;
      lastB = p.b;
      return;
    }
    // 显式禁用（如悬浮条已折叠）：整体忽略手柄，连方向/确认/返回都不响应，
    // 手柄完全交给游戏，折叠把手不会被上下/A 键驱动（#3）
    if (opts.isEnabled && !opts.isEnabled()) return;
    if (p.d !== lastD) {
      lastD = p.d;
      stopRepeat();
      const dir: Dir | null =
        p.d & 1 ? "up" : p.d & 2 ? "down" : p.d & 4 ? "left" : p.d & 8 ? "right" : null;
      if (dir) {
        movedOnce = true;
        // 方向优先级：
        // ① 滑杆聚焦时左右调值（不翻页、不切组件）；
        // ② 下拉框聚焦且已进入选择态（A）时四向切换选项；普通态不拦方向，
        //    摇杆继续空间移动（先移到下拉框，A 确认后才改值；不弹原生菜单）；
        // ③ 列表层：焦点在 [data-gp-list] 浮层里时上下逐项环绕（见 moveInList）；
        // ④ 宿主接管方向（如组件窗口左右切换页面）；
        // ⑤ 默认空间选择最近元素。
        if (adjustRange(dir)) {
          delayTimer = setTimeout(() => {
            repeatTimer = setInterval(() => adjustRange(dir), 130);
          }, 420);
        } else if (adjustSelect(dir)) {
          gpDiag(`方向 ${dir} 被下拉框消费：选中第 ${(focused as HTMLSelectElement).selectedIndex} 项`);
          delayTimer = setTimeout(() => {
            repeatTimer = setInterval(() => adjustSelect(dir), 260);
          }, 420);
        } else if (moveInList(dir)) {
          gpDiag(`列表层 ${dir} → ${focused?.textContent?.trim().slice(0, 16) ?? "?"}`);
          delayTimer = setTimeout(() => {
            repeatTimer = setInterval(() => moveInList(dir), 200);
          }, 420);
        } else if (opts.onDir?.(dir)) {
          clearFocus();
          // 切换类动作：单次触发，不连发
        } else {
          moveOrAdjust(dir);
          // 先延迟后连发（主机 UI 手感）。180ms（原 130ms）：悬浮条中段是一排
          // 紧挨着的图标按钮，130ms 一下就冲过头，握着摇杆很难停在想要的那个
          // ——实测表现为「焦点飞过去，按 A 点到的是下一个按钮」。
          delayTimer = setTimeout(() => {
            repeatTimer = setInterval(() => moveOrAdjust(dir), 180);
          }, 420);
        }
      }
    }
    if (p.a && !lastA) {
      // 必须先停掉方向连发：用户推着摇杆按 A 时，连发计时器还在跑，会把焦点从
      // 目标按钮上继续挪走（B 键分支就是这么做的，A 键之前漏了）
      stopRepeat();
      gpDiag(
        `A 按下：movedOnce=${movedOnce} focused=${focused ? focused.tagName : "null"} select=${selectMode}`
      );
      if (!movedOnce) return; // 防误触：先移动过摇杆/十字键才允许确认
      if (focused?.hasAttribute("data-gp-hold")) {
        // 长按确认类动作（关机 / 强制关闭前台）：不能走 click（组件里是空操作），
        // 改派 gp-hold-start 让组件自己跑进度条，松手时由 endHold 收尾。
        // 先 endHold()：清掉可能残留的上一次长按（松手事件此前丢过），
        // 否则 holdEl 一直非空会让之后每次按 A 都进不来（又变成「A 没反应」）
        endHold();
        holdEl = focused;
        holdStartedAt = performance.now();
        gpDiag("A 按住 → 长按确认开始");
        focused.dispatchEvent(new CustomEvent("gp-hold-start", { bubbles: true }));
      } else if (focused instanceof HTMLSelectElement) {
        // 下拉框：A = 进入/确认退出选择态（绝不能点开原生菜单——系统级的，手柄进不去）
        if (selectMode) setSelectMode(false);
        else if (focused.options.length > 0) setSelectMode(true);
      } else if (
        // 滑杆由摇杆调值，A 不点它
        !(focused instanceof HTMLInputElement && focused.type === "range")
      ) {
        focused?.click();
      }
    }
    if (p.b && !lastB) {
      stopRepeat();
      // B 分层返回：选择态 → 只退出选择（停在原下拉框）；焦点落在某控件上 →
      // 只熄灭焦点环（回到空间导航，如从下拉框退回左侧标签页）；无焦点才执行
      // 宿主返回（关设置中心 / 收悬浮条 / 回框选层）。避免「调着选项一按 B
      // 整个界面就没了」。
      // 列表层：焦点在 [data-gp-list] 浮层里（自定义下拉的候选列表）
      const inList = focused?.closest<HTMLElement>("[data-gp-list]") ?? null;
      if (selectMode) {
        setSelectMode(false);
      } else if (inList) {
        // 列表层 B：收起列表、焦点回到触发控件（不是关掉整个界面，也不是熄灭
        // 焦点环）。先派发 gp-back 让组件自己收起，再把焦点环放到触发控件上——
        // 顺序反了的话组件会当成「焦点跑到别处」，靠 200ms 轮询被动收起，
        // 而此时焦点环已经被 clearFocus 摘掉了，玩家会以为手柄失灵。
        inList.dispatchEvent(new CustomEvent("gp-back", { bubbles: true }));
        const trigger =
          inList.parentElement?.querySelector<HTMLElement>("[data-gp-list-trigger]") ?? null;
        if (trigger) focusEl(trigger);
        else clearFocus();
        gpDiag("列表层 B → 回到触发控件");
      } else if (focused && !opts.backDirect) {
        // 只熄灭焦点环（回到空间导航，如从下拉框退回左侧标签页）
        clearFocus();
      } else {
        clearFocus();
        opts.onBack();
      }
    }
    lastA = p.a;
    lastB = p.b;
    armButtonWatchdog();
  }

  let lastMouseEmit = 0;
  let mouseAnchorX = -1;
  let mouseAnchorY = -1;
  /** 鼠标一动就交还控制权：摘焦点环 + 通知宿主解锁（切回鼠标模式） */
  function onMouseActivity() {
    clearFocus();
    opts.onMouse?.();
    void emit("input://mouse", { label: getCurrentWindow().label });
  }
  const onPointer = () => onMouseActivity();
  document.addEventListener("pointerdown", onPointer);
  document.addEventListener("wheel", onPointer, { passive: true });
  // 光挪动也算「在用鼠标」——但要求真的挪了位置（累计 ≥6px）并节流，
  // 免得手柄操作时桌面轻微抖动就把模式抢走
  const onPointerMove = (e: PointerEvent) => {
    if (mouseAnchorX < 0) {
      mouseAnchorX = e.clientX;
      mouseAnchorY = e.clientY;
      return;
    }
    if (Math.abs(e.clientX - mouseAnchorX) + Math.abs(e.clientY - mouseAnchorY) < 6) return;
    const now = performance.now();
    if (now - lastMouseEmit < 200) return;
    lastMouseEmit = now;
    mouseAnchorX = e.clientX;
    mouseAnchorY = e.clientY;
    onMouseActivity();
  };
  document.addEventListener("pointermove", onPointerMove, { passive: true });

  // 别的窗口上动了鼠标也要摘环：鼠标可能没经过本窗口，本机 pointermove 收不到
  let offMouseEvent: (() => void) | null = null;
  void listen("input://mouse", () => clearFocus()).then((f) => {
    offMouseEvent = f;
  });

  // 失去系统焦点即清掉焦点环并停止连发——否则多窗口各挂残环，
  // 看起来像手柄同时控制了多个组件（窜键）；拿到系统焦点（被打开/切回）
  // 则重新上锁并落初始焦点。WebView2 里窗口 hide/show 未必触发页面的
  // visibilitychange，「本窗口获得焦点」是更可靠的「用户回来了」信号。
  void getCurrentWindow().onFocusChanged(({ payload }) => {
    if (!payload) {
      stopRepeat();
      clearFocus();
      movedOnce = false; // 失焦重置防误触状态
      // 同步清掉浏览器原生焦点环：多窗口各自的 :focus 轮廓会残留，
      // 看起来像手柄同时点亮了多个界面的控件
      const active = document.activeElement as HTMLElement | null;
      if (active && typeof active.blur === "function") active.blur();
    } else {
      armForInput(true);
    }
  });

  // 窗口隐藏/重新显示都重新上锁：hide 不销毁的窗口（设置中心等）若保留
  // 上一次的 movedOnce，重开后一颗 A 就会直接命中记忆中的按钮（窜键）。
  // placeInitial=true 时落 [data-gp-initial] 初始焦点（如设置中心的当前标签页
  // 按钮）：手柄一进来就锚在左侧导航，方向进内容、B 逐层退回，主机 UI 手感；
  // 该焦点是宿主主动放置的且落在安全默认项上，A 直接放行（危险动作仍走按住路径）。
  // 注意「拿到系统焦点」路径必须无条件落焦点——WebView2 里窗口 hide/show 的
  // 页面可见性状态可能滞后，等 visibilityState 变绿再落会永远等不到。
  function armForInput(placeInitial: boolean) {
    movedOnce = false;
    lastD = 0;
    lastA = false;
    lastB = false;
    stopRepeat();
    if (quietTimer) {
      clearTimeout(quietTimer);
      quietTimer = null;
    }
    clearFocus();
    if (!placeInitial) return;
    const initial = document.querySelector<HTMLElement>(
      "[data-gp-initial]:not(:disabled)"
    );
    if (initial && (initial.checkVisibility?.() ?? true)) {
      focusEl(initial);
      movedOnce = true;
    }
  }
  const onVisibility = () => armForInput(document.visibilityState === "visible");
  document.addEventListener("visibilitychange", onVisibility);

  // 定向监听：只收 emit_to 本窗口的事件。Rust 端按焦点/锁定模型路由，
  // 这里若用全局 listen 会把发往其它窗口的输入也收进来（窜键根源）
  void getCurrentWindow()
    .listen<GamepadInput>("gamepad://input", (e) => onInput(e.payload))
    .then((off) => {
      unlisten = off;
    });

  return () => {
    unlisten?.();
    offMouseEvent?.();
    stopRepeat();
    if (navFocus === focusEl) navFocus = null;
    if (navClear === clearFocus) navClear = null;
    if (quietTimer) {
      clearTimeout(quietTimer);
      quietTimer = null;
    }
    clearFocus();
    document.removeEventListener("visibilitychange", onVisibility);
    document.removeEventListener("pointermove", onPointerMove);
    document.removeEventListener("pointerdown", onPointer);
    document.removeEventListener("wheel", onPointer);
  };
}
