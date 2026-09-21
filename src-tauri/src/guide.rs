//! 攻略助手：一键「截屏 → 云端视觉大模型识别任务 → B 站搜攻略」。
//!
//! 流程（guide_run）：
//! 1. 干净截屏（quick::capture_screen_clean_bytes：DXGI 桌面复制，独占全屏也拿得到
//!    真实画面；隐藏本软件 UI；压成 1600px JPEG 控制请求体大小）
//! 2. 读设置中心「攻略助手」配置（guideBase/guideModel/guideKey），OpenAI 兼容
//!    chat/completions 接口，图片以 base64 data URL 上传
//! 3. 模型只输出一行搜索词（游戏名 + 画面中高亮的任务名 + 攻略）
//! 4. 打开内置浏览器（live-toolbar → live-browser）跳 B 站搜索结果页
//!
//! 后续自动跳转（live_chrome::on_page_loaded 钩子驱动，见本模块的 PENDING 状态）：
//! - 搜索页加载完 → eval 提取第一条视频链接 → 跳转
//! - 视频页加载完 → eval 读 __INITIAL_STATE__.videoData.pages → 分P 名与任务名
//!   归一化匹配 → 命中非第一集就 navigate ?p=N
//! 任何一环失败都停在上一级页面，不会白屏。

use base64::Engine;
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 等待「浏览器自动跳转」的挂起任务：(搜索词, 创建时间)。
/// 超时 120s 后钩子不再动作（用户早就不在攻略流程里了）。
static PENDING: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// 同一时刻只允许一个攻略流程
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const PENDING_TTL: Duration = Duration::from_secs(120);

fn set_pending(q: &str) {
    if let Ok(mut g) = PENDING.lock() {
        *g = Some((q.to_string(), Instant::now()));
    }
}

/// 搜索页：看一眼但不消费（视频页还要用）
fn peek_pending() -> Option<String> {
    let mut g = PENDING.lock().ok()?;
    let q = g.as_ref()?.0.clone();
    if g.as_ref()?.1.elapsed() > PENDING_TTL {
        *g = None;
        return None;
    }
    Some(q)
}

/// 视频页：消费（匹配一次即结束）
fn take_pending() -> Option<String> {
    let mut g = PENDING.lock().ok()?;
    let item = g.take()?;
    if item.1.elapsed() > PENDING_TTL {
        return None;
    }
    Some(item.0)
}

pub fn clear_pending() {
    if let Ok(mut g) = PENDING.lock() {
        *g = None;
    }
}

/// live_chrome 页面加载完成钩子：按 URL 推进「搜索页→第一条视频→分P」两跳。
/// 只在攻略流程进行中（PENDING 有值）才注入脚本，平时零开销。
pub fn on_page_loaded(win: &tauri::WebviewWindow, url: &str) {
    if url.contains("search.bilibili.com/all") {
        if peek_pending().is_none() {
            return;
        }
        // B 站搜索结果是前端渲染的，加载完成时卡片可能还没出来 →
        // 页面内 500ms 轮询最多 12s，出现第一条 BV 链接就跳转；超时停在搜索页。
        let js = r#"(function(){var tries=0;var t=setInterval(function(){tries++;try{var a=document.querySelector('a[href*="/video/BV"]');if(a){clearInterval(t);var m=a.href.match(/\/video\/(BV[^/?#]+)/);location.href=m?'https://www.bilibili.com/video/'+m[1]+'/':a.href;return;}}catch(e){}if(tries>24)clearInterval(t);},500);})()"#;
        let _ = win.eval(js);
    } else if url.contains("/video/BV") {
        let Some(q) = take_pending() else { return };
        // 分P 匹配：搜索词与每个分P 名都归一化（去标点/空白、小写），
        // 任一包含另一即命中（q=「星空 采集样本 攻略」 part=「采集样本」）。
        // 命中第 N 集（N>1）且当前没带 p= 参数才跳转，避免原地刷新循环。
        let js = format!(
            r#"(function(){{var q={q};try{{var vd=window.__INITIAL_STATE__&&window.__INITIAL_STATE__.videoData;if(!vd||!vd.pages||!q)return;function norm(s){{return String(s||'').toLowerCase().replace(/[^0-9a-z一-鿿]+/g,'');}}var nq=norm(q);if(!nq)return;var hit=-1;for(var i=0;i<vd.pages.length;i++){{var np=norm(vd.pages[i].part);if(np.length<2)continue;if(nq.indexOf(np)>=0||np.indexOf(nq)>=0){{hit=i;break;}}}}if(hit>0&&location.search.indexOf('p=')<0){{location.href=location.href.split('?')[0]+'?p='+(hit+1);}}}}catch(e){{}}}})()"#,
            q = json!(q)
        );
        let _ = win.eval(js);
    }
}

/// 读设置中心「攻略助手」配置：(base_url, model, api_key)
fn guide_cfg(app: &AppHandle) -> Result<(String, String, String), String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "无法定位数据目录".to_string())?;
    let raw = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|_| "未找到配置文件".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("配置解析失败：{e}"))?;
    let s = v.get("settings").ok_or("缺少 settings")?;
    let get = |k: &str| {
        s.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let enabled = s.get("guideEnabled").and_then(|x| x.as_bool()).unwrap_or(false);
    if !enabled {
        return Err("攻略助手未启用：请先在 设置中心 → 通用 → 攻略助手 打开开关".into());
    }
    let base = get("guideBase").trim_end_matches('/').to_string();
    let model = get("guideModel");
    let key = get("guideKey");
    if base.is_empty() || model.is_empty() || key.is_empty() {
        return Err("请先在 设置中心 → 通用 → 攻略助手 填写 Base URL / 模型 / API Key".into());
    }
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return Err("Base URL 需以 http(s):// 开头".into());
    }
    Ok((base, model, key))
}

/// 调 OpenAI 兼容接口（chat/completions）。返回首个 choice 的文本内容。
fn chat_completion(
    base: &str,
    key: &str,
    body: serde_json::Value,
    timeout: Duration,
) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let resp = agent
        .post(&format!("{base}/chat/completions"))
        .set("authorization", &format!("Bearer {key}"))
        .set("content-type", "application/json")
        .send_string(&body.to_string());
    let body: serde_json::Value = match resp {
        Ok(r) => {
            let text = r
                .into_string()
                .map_err(|e| format!("响应读取失败：{e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("响应解析失败：{e}"))?
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r
                .into_string()
                .unwrap_or_default();
            // 错误体里常带可读原因（invalid api key / model not found…），截断后透传
            let brief: String = text.chars().take(200).collect();
            return Err(format!("接口返回 {code}：{brief}"));
        }
        Err(e) => return Err(format!("请求失败：{e}")),
    };
    let content = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"));
    // content 一般是字符串；部分服务商（或思考模型）会返回 [{type:"text",…}] 数组，
    // 统一拼成纯文本。全空时把 finish_reason 带进错误信息，方便定位
    // （finish_reason="length" = max_tokens 太小被截断）。
    let text = match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        let fr = body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
            .unwrap_or("?");
        // length = 输出预算烧光：思考/推理模型（deepseek-r1、qwen-*-thinking、qvq…）
        // 会先把 token 花在隐藏的思考上，普通模型则可能真在憋长文本
        let hint = if fr == "length" {
            "——若这是思考/推理模型，请换用支持视觉的非思考模型（如 qwen-vl-max、gpt-4o），或在服务商处关闭思考模式"
        } else {
            ""
        };
        return Err(format!("模型返回空内容（finish_reason={fr}）{hint}"));
    }
    Ok(text.trim().to_string())
}

/// 模型输出的兜底清洗：可能带引号/markdown 围栏/换行/客套话。
/// 优先取「最后一行」有内容的（模型若先说废话再给词，真词在最后）；
/// 去掉「搜索词：」前缀、引号围栏，截断到 40 字。
fn sanitize_query(raw: &str) -> String {
    // 从后往前找第一行非空文本
    for line in raw.lines().rev() {
        let l = line
            .trim()
            .trim_matches('`')
            .trim_matches('"')
            .trim_matches('「')
            .trim_matches('」')
            .trim();
        if l.is_empty() {
            continue;
        }
        // 去掉「搜索词：」这类前缀
        let l = l
            .strip_prefix("搜索词")
            .or_else(|| l.strip_prefix("搜索詞"))
            .unwrap_or(l)
            .trim_start_matches(['：', ':', ' ']);
        if l.is_empty() {
            continue;
        }
        // 明显是客套/解释句（以句号结尾且较长）→ 继续往前找
        let is_prose = l.chars().count() > 20
            && (l.ends_with('。') || l.ends_with('.') || l.contains("以下是") || l.contains("搜索词"));
        if is_prose {
            continue;
        }
        let clipped: String = l.chars().take(40).collect();
        return clipped;
    }
    String::new()
}

/// 打开 / 导航内置浏览器：复用直播工具条的开窗链路（live://open-url → openTab），
/// 保证标签页、窗口镶边、手柄控制、锁定态全部走既有逻辑。
/// 工具条不存在则按悬浮条同款参数创建。显示时机不靠 win://ready 监听
/// （监听注册晚于 build，页面秒开时事件会漏掉，表现为浏览器没有顶部导航条）：
/// live-browser 出现即证明工具条页面已活过首帧（它处理了 open-url 才建出浏览器），
/// 此时直接 show 工具条，不会白框。
fn open_in_browser(app: &AppHandle, url: &str) -> Result<(), String> {
    if app.get_webview_window("live-toolbar").is_none() {
        tauri::WebviewWindowBuilder::new(
            app,
            "live-toolbar",
            tauri::WebviewUrl::App("live-toolbar.html".into()),
        )
        .title("看直播")
        .inner_size(480.0, 44.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .visible(false)
        .disable_drag_drop_handler()
        .build()
        .map_err(|e| format!("创建直播工具条失败：{e}"))?;
    }
    let _ = app.emit("live://open-url", url);
    // 等工具条页面把浏览器窗口开出来（新建时页面加载要 1~2s）；
    // 期间每 2s 重发一次：首发可能早于页面注册 listen，事件会丢
    for i in 0..24 {
        if app.get_webview_window("live-browser").is_some() {
            if let Some(tb) = app.get_webview_window("live-toolbar") {
                if !tb.is_visible().unwrap_or(true) {
                    let _ = tb.show();
                }
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
        if i % 8 == 7 {
            let _ = app.emit("live://open-url", url);
        }
    }
    Err("浏览器未能打开（工具条无响应）".into())
}

/// 攻略助手主流程（面板大按钮触发）。
/// 各阶段经 guide://status 广播：capturing / thinking / opening / done / error。
#[tauri::command]
pub async fn guide_run(app: AppHandle) -> Result<(), String> {
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("上一次攻略还在进行中".into());
    }
    let handle = app.clone();
    let r = std::thread::spawn(move || run_inner(&handle))
        .join()
        .map_err(|_| "攻略线程崩溃".to_string())?;
    RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    r
}

fn status(app: &AppHandle, phase: &str, msg: &str) {
    let _ = app.emit(
        "guide://status",
        json!({ "phase": phase, "msg": msg }),
    );
}

fn run_inner(app: &AppHandle) -> Result<(), String> {
    // 配置先行：没配好就别白截一次屏
    let (base, model, key) = guide_cfg(app)?;

    status(app, "capturing", "");
    let bytes = crate::quick::capture_screen_clean_bytes()?;
    // 诊断副本：最近一次攻略截图留在临时目录（覆盖写），排查「识别不到任务」时
    // 直接打开 %TEMP%\egb_guide\guide_last.jpg 看画面是否清晰、任务名是否可读
    let _ = std::fs::write(std::env::temp_dir().join("egb_guide").join("guide_last.jpg"), &bytes);

    let game = crate::perf::current_game_relaxed()
        .map(|g| g.name)
        .unwrap_or_default();

    status(app, "thinking", "");
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let sys = "你是游戏攻略助手。给你一张游戏截图：通常是任务/日志界面，其中高亮、选中或标记为「当前/进行中」的那一条就是玩家正在做的任务。你的输出只有一行 B 站搜索关键词，格式严格为「游戏名 任务名 攻略」，不要任何解释、引号或标点包装。规则：\
1) 先在截图中找到任务列表，读出当前任务的标题原文（英文任务名照抄英文，不要翻译）；\
2) 注意区分：鼠标指针悬停导致条目临时变亮/放大，这不是当前任务；当前任务通常带「进行中/ACTIVE/已接受」标记、专属图标或持续描边，且与右侧任务详情面板显示的标题一致——以详情面板标题为准；\
3) 游戏名用中文常用名（如 Starfield→星空、Elden Ring→艾尔登法环），没有通用中文名的保留原名；\
4) 只有当截图里完全找不到任务/目标列表时，才允许省略任务名输出「游戏名 攻略」；\
5) 总长不超过 25 个字。";
    let user_hint = if game.is_empty() {
        "（当前前台游戏进程名未知，请从截图判断）".to_string()
    } else {
        format!("（当前前台游戏进程名：{game}）")
    };
    let body = json!({
        "model": model,
        "temperature": 0.2,
        // 1024：思考/推理模型的隐藏思考也计入 max_tokens，256 会被吃光导致
        // content 为空（finish_reason=length）。普通模型一两行就停，多给不花钱。
        "max_tokens": 1024,
        "messages": [
            { "role": "system", "content": sys },
            { "role": "user", "content": [
                { "type": "text", "text": user_hint },
                { "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{b64}") } },
            ]},
        ],
    });
    let raw = chat_completion(&base, &key, body, Duration::from_secs(90))?;
    log::info!("[guide] 截图 {}KB，模型原始回复：{raw}", bytes.len() / 1024);
    let mut q = sanitize_query(&raw);
    if q.is_empty() {
        // 清洗后为空（模型输出被截断/全是包装语）：兜底至少用游戏名搜攻略，
        // 别让整个流程失败；没有游戏名才真报错
        q = if game.is_empty() {
            return Err("模型未能识别出搜索词，请重试或检查截图内容".into());
        } else {
            format!("{game} 攻略")
        };
    }

    status(app, "opening", &q);
    let url = format!(
        "https://search.bilibili.com/all?keyword={}",
        urlencoding(&q)
    );
    set_pending(&q);
    if let Err(e) = open_in_browser(app, &url) {
        clear_pending();
        return Err(e);
    }
    status(app, "done", &q);
    Ok(())
}

/// 极简 percent-encoding（UTF-8 字节级），避免再引一个依赖
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 设置中心「测试连接」：发一条纯文本消息验证 base/model/key 可用
#[tauri::command]
pub async fn guide_check(app: AppHandle) -> Result<String, String> {
    let (base, model, key) = guide_cfg(&app)?;
    let body = json!({
        "model": model,
        // 64：给思考模型留点余量，8 个 token 连思考都不够 → 空 content
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "回复 OK 两个字母即可" }],
    });
    let content = std::thread::spawn(move || {
        chat_completion(&base, &key, body, Duration::from_secs(30))
    })
    .join()
    .map_err(|_| "测试线程崩溃".to_string())??;
    Ok(content)
}
