//! 直播浏览器窗口的「窗口镶边」：
//! 浏览器加载的是远端页面，无法注入 data-tauri-drag-region，故在原生层做命中测试——
//! - 顶边热区返回 HTCAPTION：按住直播画面顶边即可直接拖动窗口；
//! - 其余四边/四角热区返回原生缩放命中码，并在 WM_SIZING 中按 16:9 校正
//!   拖出的矩形（等比例缩放，锚定对边/对角），最小 320×180（物理像素）。

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT,
    HTTOPLEFT, HTTOPRIGHT, WM_NCHITTEST, WM_SIZING, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT,
    WMSZ_BOTTOMRIGHT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
};
use tauri::webview::PageLoadEvent;
use tauri::{Emitter, Manager};

const SUBCLASS_ID: usize = 0xE6B_1C57;
const MIN_W: i32 = 320; // 最小尺寸（物理像素，16:9）
const MIN_H: i32 = 180;

/// 注入直播页面的初始化脚本（文档开始时执行）：
/// 1) _blank 链接 / window.open 就地导航——宿主会拒绝新窗口请求，表现为点击卡片无响应；
/// 2) 拦截原生全屏 API 改为「元素铺满本窗口」——WebView2 原生全屏会把整个窗口
///    放大到整块显示器盖住游戏，画中画需要的是只在窗口框内全屏；
/// 3) 心跳（live://hb，每 3s 一次，仅主框架）——渲染进程挂掉/页面假死后心跳中断，
///    工具条看门狗据此自动重载当前标签。
const LIVE_INIT: &str = r#"(() => {
  if (window.__egbPatched) return;
  window.__egbPatched = true;
  document.addEventListener('click', (e) => {
    const t = e.target;
    const a = t && t.closest ? t.closest('a[href]') : null;
    if (a && a.target && a.target !== '_self' && a.href) {
      e.preventDefault();
      e.stopPropagation();
      location.href = a.href;
    }
  }, true);
  try { window.open = (u) => { if (u) location.href = String(u); return null; }; } catch (e) {}
  const FS = 'position:fixed!important;inset:0!important;width:100vw!important;height:100vh!important;'
    + 'max-width:none!important;max-height:none!important;min-width:0!important;min-height:0!important;'
    + 'transform:none!important;margin:0!important;background:#000!important;z-index:2147483647!important;';
  let cur = null, prevStyle = null, tagged = null, savedStyles = null, purity = null;
  const fire = () => { document.dispatchEvent(new Event('fullscreenchange')); };
  const setFsEl = (v) => {
    try { Object.defineProperty(document, 'fullscreenElement', { configurable: true, get: () => v }); } catch (e) {}
    try { Object.defineProperty(document, 'webkitFullscreenElement', { configurable: true, get: () => v }); } catch (e) {}
  };
  // 这些属性会让祖先创建包含块/层叠上下文，把 fixed 定位的全屏元素困住（站点顶栏盖在画面上）
  const BLOCKERS = ['transform', 'translate', 'rotate', 'scale', 'filter', 'perspective', 'contain',
    'will-change', 'backdrop-filter', 'mix-blend-mode', 'opacity', 'zoom', 'content-visibility', 'container-type'];
  const neutral = (prop) =>
    prop === 'opacity' ? '1'
      : prop === 'mix-blend-mode' || prop === 'zoom' || prop === 'content-visibility' || prop === 'container-type' ? 'normal'
        : 'none';
  const enter = (el) => {
    if (cur === el) return;
    if (cur) leave();
    cur = el;
    prevStyle = el.style.cssText;
    el.style.cssText += ';' + FS;
    // 剧场模式（原生画中画同款效果）：页面只保留「body → 播放器」祖先链，其余分支
    // 全部 display:none。display 不可被子孙样式覆盖，站点顶栏必然消失。
    tagged = [];
    savedStyles = [];
    if (el !== document.body && el !== document.documentElement) {
      document.documentElement.style.setProperty('overflow', 'hidden', 'important');
      let p = el.parentElement;
      while (p) {
        p.setAttribute('data-egb-chain', 'a');
        tagged.push(p);
        const saved = {};
        for (const prop of BLOCKERS) {
          saved[prop] = p.style.getPropertyValue(prop);
          p.style.setProperty(prop, neutral(prop), 'important');
        }
        savedStyles.push([p, saved]);
        if (p === document.body) break;
        p = p.parentElement;
      }
      // 播放器本身也要打标记（值不同），否则会被下面的裁剪规则自己藏掉
      el.setAttribute('data-egb-chain', 'el');
      tagged.push(el);
      let st = document.getElementById('egb-style');
      if (!st) {
        st = document.createElement('style');
        st.id = 'egb-style';
        document.documentElement.appendChild(st);
      }
      st.textContent = '[data-egb-chain="a"] > :not([data-egb-chain]){display:none !important}';
      // 纯净模式：全屏元素**内部**也只留 <video>（及其祖先链）。站点水印（bilibili直播
      // +房间号）、礼物栏（电池盲盒那排）、聊天/弹幕列表、清晰度标签全在播放器容器里，
      // 上面的链式裁剪管不到——全部 display:none（无法被站点自己的样式盖回）。
      // 只留画面意味着播放器控制条也没了：Esc 退出（本脚本已处理），双击画面切全屏仍可用。
      //
      // 但这条对「点播」（B 站 /video/BV 视频页）是错的：那里没有常驻水印/礼物栏，
      // 播放器控制条（时间进度条、音量、弹幕开关、清晰度、下一P）本就在容器内，
      // 全 display:none 会把它们一起杀掉，用户既看不到进度条也拖不动。攻略助手跳的
      // 正是点播页，故点播只保留上面的「链式裁剪」（去掉站点顶栏/侧栏），不动控制条。
      const isVod = /\/video\/BV/i.test(location.href);
      const vid = isVod ? null : (el.querySelector('video') || el.querySelector('canvas'));
      if (vid && vid !== el) {
        purity = [];
        let p = vid;
        while (p && p !== el) {
          p.setAttribute('data-egb-keep', 'a');
          purity.push(p);
          p = p.parentElement;
        }
        vid.setAttribute('data-egb-keep', 'v');
        purity.push(vid);
        const all = el.querySelectorAll('*');
        for (const n of all) {
          if (n.hasAttribute('data-egb-keep') || n.hasAttribute('data-egb-chain')) continue;
          n.setAttribute('data-egb-hide', '1');
          purity.push(n);
        }
        st.textContent += ' [data-egb-hide]{display:none !important}';
      }
    }
    const g = document.getElementById('egb-resize');
    if (g) g.style.display = 'none';
    setFsEl(el);
    fire();
  };
  const leave = () => {
    if (!cur) return;
    cur.style.cssText = prevStyle || '';
    for (const item of savedStyles || []) {
      try {
        const p = item[0], saved = item[1];
        for (const prop in saved) {
          if (saved[prop]) p.style.setProperty(prop, saved[prop]);
          else p.style.removeProperty(prop);
        }
      } catch (e) {}
    }
    for (const p of tagged || []) {
      try { p.removeAttribute('data-egb-chain'); } catch (e) {}
    }
    // 纯净模式的隐藏标记一并还原（不还原的话退出全屏后页面大半是黑的）
    for (const n of purity || []) {
      try { n.removeAttribute('data-egb-hide'); n.removeAttribute('data-egb-keep'); } catch (e) {}
    }
    purity = null;
    const st = document.getElementById('egb-style');
    if (st) st.textContent = '';
    document.documentElement.style.removeProperty('overflow');
    tagged = null;
    savedStyles = null;
    const g = document.getElementById('egb-resize');
    if (g) g.style.display = '';
    cur = null;
    setFsEl(null);
    fire();
  };
  const wrapReq = (proto) => {
    if (!proto || typeof proto.requestFullscreen !== 'function') return;
    proto.requestFullscreen = function () { enter(this); return Promise.resolve(); };
    proto.webkitRequestFullscreen = function () { enter(this); return Promise.resolve(); };
  };
  try { wrapReq(Element.prototype); } catch (e) {}
  try {
    document.exitFullscreen = () => { leave(); return Promise.resolve(); };
    document.webkitExitFullscreen = () => { leave(); return Promise.resolve(); };
  } catch (e) {}
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && cur) { e.stopPropagation(); leave(); }
  }, true);
  document.addEventListener('fullscreenchange', () => {
    // 全屏状态回报给直播工具条（纯净模式：全屏时隐藏工具栏，退出后恢复）。
    // 需在 capability 中为 live-browser 的远程上下文授予 core:event:allow-emit。
    try {
      window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke('plugin:event|emit', {
        event: 'live://fs',
        payload: !!(document.fullscreenElement || document.webkitFullscreenElement),
      });
    } catch (err) {}
  });
  // 右下角等比例缩放手柄：拖动 → live://resize 回报目标宽高（16:9），工具条负责改窗口尺寸。
  // document_start 时 documentElement 可能尚未就绪，等 DOM 再挂载
  const makeGrip = () => {
    try {
      if (document.getElementById('egb-resize') || !document.documentElement) return;
      const g = document.createElement('div');
      g.id = 'egb-resize';
      g.style.cssText = 'position:fixed!important;right:0!important;bottom:0!important;'
        + 'width:30px!important;height:30px!important;z-index:2147483646!important;'
        + 'cursor:nwse-resize!important;touch-action:none!important;'
        + 'background:rgba(0,0,0,.30)!important;border-radius:8px 0 0 0!important;';
      g.innerHTML = '<svg width="30" height="30" viewBox="0 0 30 30">'
        + '<path d="M27 11 L11 27 M27 18 L18 27 M27 25 L25 27" '
        + 'stroke="rgba(255,255,255,.9)" stroke-width="2.4" stroke-linecap="round" fill="none"/></svg>';
      g.addEventListener('pointerdown', (e) => {
        e.preventDefault();
        e.stopPropagation();
        const sx = e.clientX, sy = e.clientY;
        const sw = window.innerWidth, sh = window.innerHeight;
        const sh916 = Math.round((Math.max(320, sw) * 9) / 16);
        const w0 = Math.abs(sh - sh916) < 8 ? sw : Math.round((sh * 16) / 9);
        try { g.setPointerCapture(e.pointerId); } catch (err) {}
        const move = (ev) => {
          const w = Math.max(320, Math.round(w0 + (ev.clientX - sx)));
          const h = Math.round((w * 9) / 16);
          try {
            window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke('plugin:event|emit', {
              event: 'live://resize',
              payload: { w: w, h: h },
            });
          } catch (err) {}
        };
        const up = () => {
          g.removeEventListener('pointermove', move);
          g.removeEventListener('pointerup', up);
          g.removeEventListener('pointercancel', up);
        };
        g.addEventListener('pointermove', move);
        g.addEventListener('pointerup', up);
        g.addEventListener('pointercancel', up);
      });
      document.documentElement.appendChild(g);
    } catch (err) {}
  };
  if (document.documentElement) makeGrip();
  else document.addEventListener('DOMContentLoaded', makeGrip, { once: true });
  setTimeout(makeGrip, 0);
  // 存活心跳：渲染进程一旦挂掉/被页面脚本卡死，setInterval 随之停摆，工具条据此重载。
  // 只在主框架发，避免站点 iframe 重复汇报。
  if (window.top === window) {
    const beat = () => {
      try {
        window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke('plugin:event|emit', {
          event: 'live://hb',
          payload: Date.now(),
        });
      } catch (err) {}
    };
    beat();
    setInterval(beat, 3000);
  }
})();"#;

/// 锁定观看期间是否要在直播窗口上隐藏鼠标指针。
/// 锁定后画面只用来看，指针压在视频上很碍眼（游戏里尤其明显：系统箭头会一直浮
/// 在画面上）。页面导航会重建文档、注入的样式随之丢失，故在 on_page_load 里补施。
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// 直播页面里切换「无指针」样式（锁定期间指针落在视频区域上也不显示）
fn cursor_js(on: bool) -> String {
    if on {
        r#"(()=>{try{let s=document.getElementById('egb-nocursor');if(!s){s=document.createElement('style');s.id='egb-nocursor';(document.head||document.documentElement).appendChild(s);}s.textContent='*,*::before,*::after{cursor:none!important}';}catch(e){}})()"#
            .to_string()
    } else {
        r#"(()=>{try{let s=document.getElementById('egb-nocursor');if(s)s.remove();}catch(e){}})()"#
            .to_string()
    }
}

/// 锁定 / 解锁时切换直播窗口上的鼠标指针（锁定 = 隐藏，解锁 = 恢复）。
/// 与「点击穿透」配套：穿透只管命中测试，指针形状仍由光标下的窗口决定，
/// 指针停在视频上时会露出系统箭头，所以这里再压一层 cursor:none。
#[tauri::command]
pub async fn live_cursor_hidden(app: tauri::AppHandle, on: bool) -> Result<(), String> {
    CURSOR_HIDDEN.store(on, Ordering::Relaxed);
    if let Some(win) = app.get_webview_window("live-browser") {
        win.eval(cursor_js(on)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 当前是否处于锁定态（Rust 侧唯一真相）。工具条页面重载/崩溃恢复后据此同步
/// locked 镜像：窗口的点击穿透样式与 cursor:none 都还挂着时，UI 状态不能自作主张
/// 回到「未锁定」——否则表现为锁定悄悄失效（视频仍穿透但界面当它没锁）。
#[tauri::command]
pub fn live_lock_state() -> bool {
    CURSOR_HIDDEN.load(Ordering::Relaxed)
}

/// 打开 / 导航直播浏览器窗口（隐藏创建，几何由工具条窗口负责）。
/// 带初始化脚本与回调：live://nav 回报地址（标签页跟踪），
/// live://load 回报页面加载开始/结束（地址栏加载中状态）。
#[tauri::command]
pub async fn live_browser_open(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let parsed = tauri::Url::parse(&url).map_err(|e| format!("地址无效：{e}"))?;
    if !matches!(parsed.scheme(), "http" | "https" | "about") {
        return Err(format!("不支持的地址：{url}"));
    }
    if let Some(win) = app.get_webview_window("live-browser") {
        return win.navigate(parsed).map_err(|e| e.to_string());
    }
    let handle = app.clone();
    let load_handle = app.clone();
    tauri::WebviewWindowBuilder::new(&app, "live-browser", tauri::WebviewUrl::External(parsed))
        .title("直播")
        .min_inner_size(320.0, 180.0)
        // 必须 false：decorations(false)+resizable(true) 时 Windows 仍在四边留
        // ~8px 不可见拉伸带（HTTOP 命中区），光标没到边缘就变 ↕、点击被当成
        // 拖边框吞掉，正好盖住网页顶部导航。缩放全部走程序化 setSize
        //（工具条按钮 / 注入脚本右下角手柄 / RT+LT），不依赖原生拖边框。
        .resizable(false)
        .decorations(false)
        .transparent(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(true)
        .visible(false)
        .disable_drag_drop_handler()
        .initialization_script(LIVE_INIT)
        .on_navigation(move |nav| {
            // 手柄 B 返回链的历史深度记账（live_pad）
            crate::live_pad::note_nav();
            let _ = handle.emit("live://nav", nav.to_string());
            true
        })
        .on_page_load(move |win, payload| {
            let phase = match payload.event() {
                PageLoadEvent::Started => "start",
                PageLoadEvent::Finished => "end",
            };
            // 锁定期间导航/重载会重建文档（注入的隐藏样式随之丢失）→ 加载完成后补施一次
            if phase == "end" && CURSOR_HIDDEN.load(Ordering::Relaxed) {
                let _ = win.eval(cursor_js(true));
            }
            // 攻略助手：B 站搜索页/视频页加载完 → 自动跳第一条视频 / 匹配分P
            if phase == "end" {
                crate::guide::on_page_loaded(&win, &payload.url().to_string());
            }
            let _ = load_handle.emit(
                "live://load",
                serde_json::json!({ "url": payload.url().to_string(), "phase": phase }),
            );
        })
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 直播页面前进 / 后退（对远程页面执行 history 导航）
#[tauri::command]
pub async fn live_browser_nav(app: tauri::AppHandle, dir: String) -> Result<(), String> {
    let win = app
        .get_webview_window("live-browser")
        .ok_or("live-browser 窗口不存在")?;
    let js = match dir.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        _ => return Err(format!("未知方向：{dir}")),
    };
    win.eval(js).map_err(|e| e.to_string())
}

/// 给 live-browser 窗口挂上拖动热区与等比例缩放（可重复调用，按 hwnd 生效）
#[tauri::command]
pub async fn attach_live_chrome(app: tauri::AppHandle) -> Result<(), String> {
    let win = app
        .get_webview_window("live-browser")
        .ok_or("live-browser 窗口不存在")?;
    let hwnd_raw = win.hwnd().map_err(|e| e.to_string())?.0 as isize;
    // 注意：这里不做 DWMWCP_ROUND——工具条贴在浏览器正上方，四角圆角会把
    // 接缝两端的顶角裁圆，露出后面一层形成「缺角」。一体化后外露的顶部两角
    // 由工具条自身的 CSS 圆角负责。
    // 子类化必须在窗口所属线程（主线程）执行
    app.run_on_main_thread(move || unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let ok = SetWindowSubclass(hwnd, Some(subproc), SUBCLASS_ID, 0).as_bool();
        if !ok {
            log::warn!("live-browser 子类化失败（窗口镶边不可用）");
        }
    })
    .map_err(|e| e.to_string())
}

unsafe extern "system" fn subproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            if let Some(ht) = hit_test(hwnd, lparam) {
                return LRESULT(ht as isize);
            }
        }
        WM_SIZING => {
            if sizing(lparam, wparam) {
                return LRESULT(1);
            }
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// 顶边 → HTCAPTION（拖动）；其余边/角 → 缩放命中码；内部 → None（走默认客户区处理）
unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rc);
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    // lParam 低/高 16 位为屏幕坐标（有符号，跨副屏可为负）
    let sx = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let sy = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    let edge = (10 * GetDpiForWindow(hwnd) / 96).max(6) as i32;
    let dx = sx - rc.left;
    let dy = sy - rc.top;
    let left = dx < edge;
    let right = dx >= w - edge;
    let top = dy < edge;
    let bottom = dy >= h - edge;
    let ht = match (top, bottom, left, right) {
        (true, _, true, _) => HTTOPLEFT,
        (true, _, _, true) => HTTOPRIGHT,
        (false, true, true, _) => HTBOTTOMLEFT,
        (false, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTCAPTION, // 顶边：拖动直播画面
        (false, true, _, _) => HTBOTTOM,
        (_, _, true, _) => HTLEFT,
        (_, _, _, true) => HTRIGHT,
        _ => return None,
    };
    Some(ht)
}

/// WM_SIZING：把拖出的矩形校正为 16:9（锚定对边/对角），返回 true 表示已改写矩形
unsafe fn sizing(lparam: LPARAM, wparam: WPARAM) -> bool {
    let r = &mut *(lparam.0 as *mut RECT);
    let edge = wparam.0 as u32;
    let mut w = (r.right - r.left).max(MIN_W);
    let mut h = (r.bottom - r.top).max(MIN_H);
    match edge {
        WMSZ_LEFT | WMSZ_RIGHT => h = w * 9 / 16,
        WMSZ_TOP | WMSZ_BOTTOM => w = h * 16 / 9,
        // 角：按拖拽更明显的方向推导
        _ => {
            if w * 9 / 16 >= h {
                h = w * 9 / 16;
            } else {
                w = h * 16 / 9;
            }
        }
    }
    w = w.max(MIN_W);
    h = h.max(MIN_H);
    match edge {
        WMSZ_LEFT => {
            r.left = r.right - w;
            r.bottom = r.top + h;
        }
        WMSZ_RIGHT => {
            r.right = r.left + w;
            r.bottom = r.top + h;
        }
        WMSZ_TOP => {
            r.top = r.bottom - h;
            r.right = r.left + w;
        }
        WMSZ_BOTTOM => {
            r.bottom = r.top + h;
            r.right = r.left + w;
        }
        WMSZ_TOPLEFT => {
            r.left = r.right - w;
            r.top = r.bottom - h;
        }
        WMSZ_TOPRIGHT => {
            r.right = r.left + w;
            r.top = r.bottom - h;
        }
        WMSZ_BOTTOMLEFT => {
            r.left = r.right - w;
            r.bottom = r.top + h;
        }
        WMSZ_BOTTOMRIGHT => {
            r.right = r.left + w;
            r.bottom = r.top + h;
        }
        _ => return false,
    }
    true
}
