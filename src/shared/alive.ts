//! 存活心跳：回应 Rust 看门狗（src-tauri/src/watchdog.rs）的 app://ping。
//! 显卡驱动超时重置（TDR）后窗口可能变成空白（句柄在、JS 不跑），
//! 看门狗据此判定 webview 已死并自动重载。各窗口入口调一次即可。

import { emit, listen } from "@tauri-apps/api/event";

export async function startAlive(label: string) {
  const pong = () => void emit("app://pong", { label });
  await listen("app://ping", pong);
  pong(); // 上线即报一次
}
