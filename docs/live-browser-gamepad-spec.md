# 直播浏览器手柄控制模块 — 需求规格（用户原文固化，实施契约）

> 来源：2026-09-20 用户消息（25 节主规格 + 追加的「锁定/语音」细化）。
> 前提铁律：**不要重做/重写现有 Game Bar**，只在现有「直播」功能上新增本模块；
> 不修改现有 UI 整体设计与布局，只加必要的状态提示 UI。

## 核心流程
游戏 → 打开 Game Bar → 点「直播」→ 打开直播浏览器 → 手柄控制浏览器。
浏览器控制状态下：**手柄输入不能同时被游戏消费**（单路由，不分流）。

## 一、ControllerInputRouter（独立于 Windows 焦点）
- 不能靠 `WebView2.Focus()` / `SetForegroundWindow` 解决输入归属；
- 必须有独立的 ControllerInputRouter：BrowserControl=false → Game；true → BrowserController；
- 不强制 WebView2 成为前台窗口（避免游戏失焦/暂停/最小化）。

## 二、CursorRouter：真实鼠标与手柄虚拟鼠标共存
- 两种光标来源：物理鼠标、手柄虚拟鼠标；不得互相争夺同一光标位置；
- 「最后操作设备优先」：检测到真实 MouseMove → PhysicalMouse；左摇杆移动 → ControllerMouse（显示虚拟鼠标）；
- **拖动期间（A Down 未抬起）锁定输入源为 Controller**，物理鼠标移动不得抢走控制权，A Up 解锁。

## 三、VirtualCursor（左摇杆虚拟鼠标）
- Dead Zone 0.10~0.15；非线性速度：`speed = BaseSpeed * pow(abs(normalized), 1.5)`；禁止 `x += stickX` 线性累加；
- 平滑、有最大速度、支持灵敏度设置、可显隐；
- 坐标换算实时计算：屏幕坐标 → Overlay 坐标 → WebView2 Client 坐标；按当前显示器分辨率/DPI/Overlay 位置/WebView2 尺寸，**不得写死 1920×1080**。

## 四、按键映射（浏览器控制状态）
- A Down/Up = 鼠标左键 Down/Up；A + 左摇杆 = 标准 MouseDown→MouseMove→MouseUp 拖动（进度条/音量条/滑块/滚动条/拖放，通用，不写死网站）；
- LT + 左摇杆 = 移动浏览器窗口（此时左摇杆**不**动鼠标；松 LT 恢复）；
- LT 按住 = 连续缩小、RT 按住 = 连续放大；最小 320×180，最大约屏幕 90~95%，保持宽高比；LT+左摇杆同时发生时优先移动、不同时缩放；
- 右摇杆 ↑↓ = 网页滚动（dead zone + 加速度 + 平滑）；←→ = HTML5 video currentTime ∓10s（页面无 video 则不执行）；
- Y = 播放/暂停：优先找可见/正在播的 `document.querySelectorAll("video")`，`paused ? play() : pause()`；找不到回退模拟 Space；
- B 短按 = 返回，优先级：正在输入→取消输入 ＞ 全屏→退出全屏 ＞ 有历史→浏览器 Back ＞ 退出 Browser Control。

## 五、状态机
GAME_MODE / BROWSER_MODE / WINDOW_MOVE_MODE / DRAG_MODE / VOICE_MODE。
转换：点直播→BROWSER_MODE；LT+左摇杆→WINDOW_MOVE；A Down→DRAG；B Hold→VOICE；A Up/LT Up/语音完成→回 BROWSER_MODE。

## 六、语音（Push-to-Talk）
- B 按下超 500ms → 开始录音；松开 → 停止 → STT → Command Parser → 执行；
- 架构：Voice → SpeechToText → Command Parser → Structured Command → BrowserController；
  AI（若参与）只做自然语言→Intent，**不得**让 AI 直接 SendInput/PowerShell/CMD/系统命令；
- 命令集（只需常用）：play/pause、seek(±秒，如「快进一分钟」)、resize(放大缩小 1.1 等)、move(top-right 等方位)、fullscreen/exit、输入文字（「请输入 xxx」→ 找 input/textarea/contenteditable → focus → 输入；找不到提示「先用左摇杆+A 点击输入框」）；
- **语音期间 B 键必须被 Router 消费，游戏不能同时收到 B**；
- 语音 UI（直播窗口附近浮动，半透明、不挡游戏画面、自动出现/消失、无需点击）：
  Idle 不显示 → Listening「🎙 正在听...」+音量波形 → Recognizing「正在识别...」→ Success「✓ 已执行」/ Error「× 没听清，请重试」，成功/失败 1~2s 自动消失。

## 七、锁定 / 取消锁定（追加细化，与主规格冲突处以本节为准）
- 打开直播后：浏览器正常获得焦点，鼠标+手柄都可操作网页（= 主规格的完整手柄浏览器控制）；
- 用户选好直播后点「**锁定**」：`BrowserControlLocked=true, BrowserVisible=true, GameFocused=true`
  - 浏览器继续显示在游戏上方、继续播放；WebView2 不刷新/不重载/不销毁；
  - Windows 焦点交还游戏，游戏恢复键盘/手柄操作；ControllerInputRouter 继续工作；
  - 锁定态下游戏正常收普通输入；**B 长按语音仍可用**（不要求浏览器重获 Windows 焦点）；
  - ⚠️ 与主规格第十八节「锁定后全部摇杆/ABY/LT/RT 都给浏览器、游戏收不到」存在张力：
    追加节语义 = 锁定态主要是「游戏正常玩 + 语音控制直播」；
    需与用户确认：锁定态是否保留摇杆/A/LT/RT 等手柄控制（全拦截），还是仅语音拦截（B 长按）。
- 「**取消锁定**」：`BrowserControlLocked=false, BrowserVisible=true, BrowserFocused=true, GameFocused=false`，浏览器重获焦点可继续交互，不刷新不重载；
- 三态分离：**浏览器是否显示 / Windows 焦点 / 控制器输入路由** 不得混为一个状态。

## 八、WebView2
- 创建后一直复用：改 Position/Size/Visibility/InputMode，**不得**因缩放/移动/切换控制态而 Reload；
- 异常处理：初始化失败、崩溃、页面加载失败、禁止嵌入、GPU/TDR 渲染丢失、游戏切分辨率、DPI 变化、多显示器；渲染失败时尝试重建 WebView2，但不得让 Game Bar 崩溃。

## 九、实现顺序（用户指定）
1. ControllerInputRouter 2. CursorRouter 3. VirtualMouse 4. BrowserController 5. BrowserWindowController 6. WebView2 输入处理 7. 最后接入现有「直播」按钮。

## 关键技术难点（实施前须知）
- XInput 是**进程轮询**模型，与 Windows 焦点无关：游戏即使不在前台也会继续读手柄。
  要做到「锁定后手柄不被游戏消费」，唯一可靠途径是在游戏进程内 hook XInputGetState/XInputSetState
  （本项目已有 egb_hook.dll 注入 + vtable detour 基础设施，见 overlay_inject.rs / hooks/egb_hook）。
- STT 引擎未指定（候选：系统 SAPI/Windows.Media.SpeechRecognition、云端、本地 whisper.cpp）——需用户决策。
