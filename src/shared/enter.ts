import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { WebviewWindow } from "@tauri-apps/api/webviewWindow";

/**
 * 窗口内容入场：等窗口真正可见后再给根元素加 .enter 类，过渡动画随即播放。
 *
 * 不能用普通 CSS animation：组件/弹窗/设置中心都是预创建的隐藏窗口，页面在
 * show() 之前就加载完，动画会在用户看见之前放完，等于没动画。
 * 双 rAF 确保初始样式先完成一帧渲染，transition 才会生效。
 */
export function enterOnShow(el: HTMLElement, onEnter?: () => void): void {
  const play = () =>
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        el.classList.add("enter");
        onEnter?.();
      })
    );
  if (document.visibilityState === "visible") {
    play();
    return;
  }
  const onVis = () => {
    if (document.visibilityState !== "visible") return;
    document.removeEventListener("visibilitychange", onVis);
    clearInterval(poll);
    play();
  };
  document.addEventListener("visibilitychange", onVis);
  // 轮询兜底：窗口隐藏期间整页重载（vite 热更 / 看门狗）后，visibilitychange
  // 偶发不再触发，.enter 永远加不上 → 面板以 opacity:0 卡成一块空黑框。
  // 上限 6s：纯兜底，正常路径仍走事件。
  const t0 = Date.now();
  const poll = setInterval(() => {
    if (el.classList.contains("enter") || Date.now() - t0 > 6000) {
      clearInterval(poll);
      return;
    }
    if (document.visibilityState === "visible") {
      clearInterval(poll);
      play();
    }
  }, 150);
}

/**
 * 子窗口首帧绘制完成上报（win://ready）：悬浮条收到后才 show 窗口，
 * 避免在 WebView2 画出第一帧前 show，露出未绘制的白底（白框闪现）。
 * 双 rAF 保证 DOM 已提交渲染管线。
 */
export function notifyReady(): void {
  requestAnimationFrame(() =>
    requestAnimationFrame(() => void emit("win://ready", getCurrentWindow().label))
  );
}

/**
 * 等 label 窗口上报首帧后再显示；show 之后页面再走 enterOnShow 弹入，
 * 打开过程 = 透明出现 → 面板弹出，全程无白框。
 * timeoutMs 兜底：ready 事件万一丢失也照常显示，不会永久不可见。
 */
export function showWhenReady(
  win: WebviewWindow,
  label: string,
  opts: { focus?: boolean; timeoutMs?: number } = {}
): void {
  const focus = opts.focus ?? true;
  let done = false;
  let off: (() => void) | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  const finish = () => {
    if (done) return;
    done = true;
    off?.();
    if (timer) clearTimeout(timer);
    win.show().catch(() => {});
    if (focus) win.setFocus().catch(() => {});
  };
  timer = setTimeout(finish, opts.timeoutMs ?? 2500);
  void listen<string>("win://ready", (e) => {
    if (e.payload === label) finish();
  }).then((f) => {
    if (done) f();
    else off = f;
  });
}
