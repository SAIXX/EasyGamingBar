<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import WIcon from "../../shared/WIcon.vue";
import { skinStyle } from "../../shared/skins";
import { startGamepadNav } from "../../shared/gamepad";
import { applySelfGlass } from "../../shared/glass";
import { enterOnShow, notifyReady } from "../../shared/enter";
import { t } from "../../shared/i18n";
import {
  connectWifi,
  disconnectWifi,
  getDeviceVolume,
  getMicMuted,
  getRadioStatus,
  getWifiStatus,
  listAudioDevices,
  listWifiNetworks,
  loadConfig,
  openSettingsCenter,
  openSystemPage,
  setAirplaneMode,
  setBtEnabled,
  setDeviceVolume,
  setWifiEnabled,
  showVirtualKeyboard,
  toggleMicMute,
  type AppConfig,
  type RadioStatus,
  type WifiNetwork,
  type WifiStatus,
} from "../../shared/api";

const config = ref<AppConfig>({ settings: {}, games: [], widgets: {} });
const panelSkin = computed(() => skinStyle(config.value.settings, 0.18));
const rootEl = ref<HTMLElement | null>(null);

/* ---------------- 二级菜单导航 ---------------- */

type View = "main" | "network" | "bluetooth";
const view = ref<View>("main");
const stack: View[] = [];
function go(v: View) {
  stack.push(view.value);
  view.value = v;
}
function back() {
  view.value = stack.pop() ?? "main";
}

/* ---------------- 麦克风 ---------------- */

const micMuted = ref(true);

async function refreshMic() {
  try {
    micMuted.value = await getMicMuted();
  } catch (e) {
    console.error("读取麦克风状态失败", e);
  }
}

async function onMicButton() {
  try {
    micMuted.value = await toggleMicMute();
  } catch (e) {
    console.error("切换麦克风失败", e);
  }
}

/* ---------------- 音量（默认输出设备） ---------------- */

const outVol = ref(32);
const outMuted = ref(false);
let outId: string | null = null;
let volTimer: ReturnType<typeof setTimeout> | null = null;

async function refreshVolume() {
  try {
    const list = await listAudioDevices("output");
    const def = list.find((d) => d.is_default);
    if (!def) return;
    outId = def.id;
    const v = await getDeviceVolume(def.id, "output");
    outVol.value = v.volume;
    outMuted.value = v.muted;
  } catch (e) {
    console.error("读取音量失败", e);
  }
}

function onVolSlider(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value);
  outVol.value = v;
  outMuted.value = false;
  if (!outId) return;
  if (volTimer) clearTimeout(volTimer);
  volTimer = setTimeout(
    () => setDeviceVolume(outId!, "output", { volume: v }).catch(() => {}),
    60
  );
}

async function onVolIcon() {
  if (!outId) return;
  outMuted.value = !outMuted.value;
  await setDeviceVolume(outId, "output", { mute: outMuted.value }).catch(() => {});
}

/* ---------------- 键盘 / 更多设置 ---------------- */

const kbErr = ref("");
let kbTimer: ReturnType<typeof setTimeout> | null = null;
async function onKeyboard() {
  kbErr.value = "";
  if (kbTimer) clearTimeout(kbTimer);
  try {
    await showVirtualKeyboard();
  } catch (e) {
    // 不能只 console.error —— 静默失败在界面上就是"点了没反应"
    kbErr.value = String(e ?? t("fs.keyboardFail"));
    kbTimer = setTimeout(() => (kbErr.value = ""), 5000);
  }
}

async function close() {
  // 隐藏复用而非销毁：面板页面常驻，重开瞬时
  await getCurrentWindow().hide();
}

async function openMore() {
  // 设置中心的 show/创建统一走 Rust 命令（独立线程处理）——此前从本面板直接
  // WebviewWindow.show() 会与窗口线程互等，点「更多设置」整窗卡死
  void openSettingsCenter().catch((e) => console.error(t("fs.openSettingsFail"), e));
  await close();
}

/* ---------------- 无线电：Wi-Fi / 蓝牙 / 飞行模式 ---------------- */

const radios = ref<RadioStatus>({ wifi_on: true, bt_on: false, airplane: false });
const wifi = ref<WifiStatus>({
  radio_on: true,
  connection: { connected: false, ssid: "", signal: 0 },
});
const networks = ref<WifiNetwork[]>([]);
const netsLoading = ref(false);
const busy = ref<"" | "wifi" | "bt" | "airplane">("");
const connecting = ref(""); // 正在连接的 ssid
const pwdFor = ref(""); // 展开密码输入框的 ssid
const password = ref("");
const netError = ref("");

async function refreshRadios() {
  try {
    radios.value = await getRadioStatus();
  } catch (e) {
    console.error("读取无线电状态失败", e);
  }
}

async function refreshWifi(withNetworks = true) {
  try {
    wifi.value = await getWifiStatus();
    if (!withNetworks) return;
    netsLoading.value = true;
    networks.value = wifi.value.radio_on ? await listWifiNetworks() : [];
  } catch (e) {
    console.error("读取 Wi-Fi 状态失败", e);
  } finally {
    netsLoading.value = false;
  }
}

async function toggleWifi() {
  if (busy.value) return;
  busy.value = "wifi";
  netError.value = "";
  try {
    await setWifiEnabled(!radios.value.wifi_on);
    await refreshRadios();
    await refreshWifi();
  } catch (e) {
    netError.value = String(e);
  } finally {
    busy.value = "";
  }
}

async function toggleBt() {
  if (busy.value) return;
  busy.value = "bt";
  netError.value = "";
  try {
    await setBtEnabled(!radios.value.bt_on);
    await refreshRadios();
  } catch (e) {
    netError.value = String(e);
  } finally {
    busy.value = "";
  }
}

async function toggleAirplane() {
  if (busy.value) return;
  busy.value = "airplane";
  netError.value = "";
  try {
    await setAirplaneMode(!radios.value.airplane);
    await refreshRadios();
    await refreshWifi();
  } catch (e) {
    netError.value = String(e);
  } finally {
    busy.value = "";
  }
}

async function pickNetwork(n: WifiNetwork) {
  if (n.connected || connecting.value) return;
  netError.value = "";
  connecting.value = n.ssid;
  pwdFor.value = "";
  password.value = "";
  try {
    // 已存配置的网络直接连；需要密码时后端会报错，再展开密码框
    await connectWifi(n.ssid);
    await refreshWifi();
  } catch (e) {
    const msg = String(e);
    if (msg.includes("密码")) {
      pwdFor.value = n.ssid;
    } else {
      netError.value = msg;
    }
  } finally {
    connecting.value = "";
  }
}

async function connectWithPassword(ssid: string) {
  if (connecting.value || !password.value) return;
  netError.value = "";
  connecting.value = ssid;
  try {
    await connectWifi(ssid, password.value);
    pwdFor.value = "";
    password.value = "";
    await refreshWifi();
  } catch (e) {
    netError.value = String(e);
  } finally {
    connecting.value = "";
  }
}

async function disconnect() {
  if (!wifi.value.connection.connected) return;
  await disconnectWifi().catch(() => {});
  await refreshWifi();
}

function netSub() {
  if (!radios.value.wifi_on) return t("fs.wifiOff");
  return wifi.value.connection.connected
    ? wifi.value.connection.ssid
    : t("fs.notConnected");
}

/* ---------------- 信号格 ---------------- */

const signalLevel = (s: number) => (s >= 75 ? 4 : s >= 50 ? 3 : s >= 25 ? 2 : s > 0 ? 1 : 0);

let unlisten: Array<() => void> = [];
onMounted(() => {
  void applySelfGlass(); // 面板：Acrylic 优先
  if (rootEl.value) enterOnShow(rootEl.value);
  notifyReady(); // 首帧就绪 → 悬浮条才 show 本窗口
  // 手柄：B 返回 = 关闭面板
  unlisten.push(startGamepadNav({ onBack: close }));
});
onMounted(async () => {
  config.value = await loadConfig();
  await Promise.all([refreshMic(), refreshVolume(), refreshRadios()]);
  await refreshWifi();

  // 快捷键/其他窗口改动了系统状态 → 刷新
  unlisten.push(
    await listen<{ muted?: boolean }>("sys://mic", (e) => {
      if (typeof e.payload?.muted === "boolean") micMuted.value = e.payload.muted;
    })
  );
  unlisten.push(
    await listen("sys://radios", async () => {
      await refreshRadios();
      await refreshWifi();
    })
  );
  unlisten.push(
    await listen<unknown>("config://updated", async () => {
      config.value = await loadConfig();
    })
  );
  const off = await getCurrentWindow().onFocusChanged(({ payload }) => {
    if (!payload) close();
  });
  unlisten.push(off);
});
onBeforeUnmount(() => {
  unlisten.forEach((f) => f());
  if (volTimer) clearTimeout(volTimer);
});
</script>

<template>
  <div ref="rootEl" class="panel" :style="panelSkin">
    <header>
      <button v-if="view !== 'main'" class="backbtn" @click="back">
        <WIcon name="chevL" :size="16" />
      </button>
      <span class="title">
        <template v-if="view === 'main'"
          ><WIcon name="gear" :size="16" /> {{ t("fs.settings") }}</template
        >
        <template v-else-if="view === 'network'">{{ t("fs.network") }}</template>
        <template v-else>{{ t("fs.bluetooth") }}</template>
      </span>
      <button class="x" @click="close"><WIcon name="close" :size="15" /></button>
    </header>

    <!-- ============ 主面板 ============ -->
    <div v-if="view === 'main'" class="body">
      <button class="accent" :class="{ on: !micMuted }" @click="onMicButton">
        <WIcon name="mic" :size="15" /> {{ micMuted ? t("fs.micOn") : t("fs.micOff") }}
      </button>

      <div class="volrow">
        <button
          class="volicon"
          :class="{ muted: outMuted }"
          :title="t('fs.muteToggle')"
          @click="onVolIcon"
        >
          <WIcon name="volume" :size="16" />
        </button>
        <input type="range" min="0" max="100" :value="outVol" @input="onVolSlider" />
        <span class="volnum">{{ outMuted ? t("fs.muted") : outVol }}</span>
      </div>

      <button class="dark" @click="onKeyboard">
        <WIcon name="keyboard" :size="15" /> {{ t("fs.keyboard") }}
      </button>
      <div v-if="kbErr" class="kberr">{{ kbErr }}</div>

      <div class="sep"></div>

      <button class="row link" @click="openMore">
        <span class="ric"><WIcon name="gear" :size="16" /></span>
        <span class="rtext rname">{{ t("fs.more") }}</span>
        <WIcon name="chevR" :size="14" />
      </button>

      <button class="row link" @click="go('network')">
        <span class="ric"><WIcon name="wifi" :size="16" /></span>
        <div class="rtext">
          <div class="rname">{{ t("fs.network") }}</div>
          <div class="rsub">{{ netSub() }}</div>
        </div>
        <WIcon name="chevR" :size="14" />
      </button>

      <button class="row link" @click="go('bluetooth')">
        <span class="ric"><WIcon name="bluetooth" :size="16" /></span>
        <div class="rtext">
          <div class="rname">{{ t("fs.bluetooth") }}</div>
          <div class="rsub">{{ radios.bt_on ? t("common.on") : t("common.off") }}</div>
        </div>
        <WIcon name="chevR" :size="14" />
      </button>

      <div class="row">
        <span class="ric"><WIcon name="airplane" :size="16" /></span>
        <div class="rtext">
          <div class="rname">{{ t("fs.airplane") }}</div>
          <div class="rsub">{{ radios.airplane ? t("common.on2") : t("common.off2") }}</div>
        </div>
        <button
          class="switch"
          :class="{ on: radios.airplane }"
          :disabled="busy !== ''"
          @click="toggleAirplane"
        >
          <span class="knob"></span>
        </button>
      </div>
    </div>

    <!-- ============ 网络（二级） ============ -->
    <div v-else-if="view === 'network'" class="body">
      <div class="row">
        <span class="ric"><WIcon name="wifi" :size="16" /></span>
        <div class="rtext">
          <div class="rname">Wi-Fi</div>
          <div class="rsub">{{ radios.wifi_on ? t("common.on") : t("common.off") }}</div>
        </div>
        <button
          class="switch"
          :class="{ on: radios.wifi_on }"
          :disabled="busy !== ''"
          @click="toggleWifi"
        >
          <span class="knob"></span>
        </button>
      </div>

      <template v-if="radios.wifi_on">
        <div v-if="wifi.connection.connected" class="curconn">
          <WIcon name="wifi" :size="15" />
          <div class="rtext">
            <div class="rname">{{ wifi.connection.ssid }}</div>
            <div class="rsub">
              {{ t("fs.connectedSignal", { n: wifi.connection.signal }) }}
            </div>
          </div>
          <button class="mini" :disabled="connecting !== ''" @click="disconnect">
            {{ t("fs.disconnect") }}
          </button>
        </div>

        <div class="listhead">
          <span>{{ t("fs.available") }}</span>
          <button
            class="refresh"
            :disabled="netsLoading"
            :title="t('fs.refresh')"
            @click="refreshWifi()"
          >
            <WIcon name="search" :size="13" />
          </button>
        </div>

        <div v-if="netError" class="err">{{ netError }}</div>
        <div v-if="netsLoading" class="hint">{{ t("fs.scanning") }}</div>
        <div v-else-if="networks.length === 0" class="hint">{{ t("fs.noNetworks") }}</div>

        <div v-for="n in networks" :key="n.ssid" class="net" :class="{ sel: pwdFor === n.ssid }">
          <button class="netrow" @click="pickNetwork(n)">
            <span class="sig" :data-l="signalLevel(n.signal)"><i /><i /><i /><i /></span>
            <span class="rtext">
              <span class="rname">{{ n.ssid }}</span>
              <span v-if="n.connected" class="rsub conn">{{ t("fs.connected") }}</span>
            </span>
            <span v-if="n.auth !== 'open'" class="lock"><WIcon name="lock" :size="12" /></span>
            <span v-if="connecting === n.ssid" class="wait">{{ t("fs.connecting") }}</span>
          </button>
          <div v-if="pwdFor === n.ssid" class="pwdrow">
            <input
              v-model="password"
              type="password"
              :placeholder="t('fs.passwordPh')"
              @keyup.enter="connectWithPassword(n.ssid)"
            />
            <button class="mini primary" :disabled="!password" @click="connectWithPassword(n.ssid)">
              {{ t("fs.connect") }}
            </button>
            <button class="mini" @click="pwdFor = ''">{{ t("common.cancel") }}</button>
          </div>
        </div>

        <button class="row link" @click="openSystemPage('network-wifi')">
          <span class="ric"><WIcon name="switch" :size="15" /></span>
          <span class="rtext rname">{{ t("fs.openNetSettings") }}</span>
          <WIcon name="chevR" :size="14" />
        </button>
      </template>
    </div>

    <!-- ============ 蓝牙（二级） ============ -->
    <div v-else class="body">
      <div class="row">
        <span class="ric"><WIcon name="bluetooth" :size="16" /></span>
        <div class="rtext">
          <div class="rname">{{ t("fs.bluetooth") }}</div>
          <div class="rsub">{{ radios.bt_on ? t("common.on") : t("common.off") }}</div>
        </div>
        <button
          class="switch"
          :class="{ on: radios.bt_on }"
          :disabled="busy !== ''"
          @click="toggleBt"
        >
          <span class="knob"></span>
        </button>
      </div>

      <div v-if="netError" class="err">{{ netError }}</div>
      <div v-if="busy === 'bt'" class="hint">{{ t("fs.switching") }}</div>

      <div class="sep"></div>

      <button class="row link" @click="openSystemPage('bluetooth')">
        <span class="ric"><WIcon name="switch" :size="15" /></span>
        <div class="rtext">
          <div class="rname">{{ t("fs.manageDevices") }}</div>
          <div class="rsub">{{ t("fs.manageDevicesSub") }}</div>
        </div>
        <WIcon name="chevR" :size="14" />
      </button>

      <div class="note">{{ t("fs.btWarn") }}</div>
    </div>
  </div>
</template>

<style scoped>
.panel {
  width: 100vw;
  height: 100vh;
  display: flex;
  flex-direction: column;
  background: var(--panel-bg-dense);
  border: 1px solid var(--panel-border);
  /* 8px = Win11 DWM 系统圆角半径，与窗口圆角化（glass.rs）对齐 */
  border-radius: 8px;
  overflow: hidden;
  box-shadow: var(--panel-inset), 0 10px 40px rgba(0, 0, 0, 0.5);
  /* 弹出入场：等窗口可见后由 enterOnShow 加 .enter 播放 */
  opacity: 0;
  transform: translateY(12px) scale(0.96);
  transition: opacity 0.22s ease, transform 0.32s cubic-bezier(0.3, 1.35, 0.4, 1);
}
.panel.enter {
  opacity: 1;
  transform: none;
}

header {
  flex: none;
  display: flex;
  align-items: center;
  gap: 6px;
  height: 44px;
  padding: 0 10px 0 16px;
}
.title {
  flex: 1;
  display: inline-flex;
  align-items: center;
  gap: 9px;
  font-size: 13.5px;
  font-weight: 600;
  color: #f2f3f5;
}
.backbtn,
.x {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: rgba(255, 255, 255, 0.65);
  cursor: pointer;
}
.backbtn:hover,
.x:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #fff;
}

.body {
  flex: 1;
  overflow-y: auto;
  padding: 2px 12px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.accent,
.dark {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: 34px;
  border-radius: 8px;
  font-size: 13px;
  cursor: pointer;
  border: none;
}
.accent {
  background: #d7dae0;
  color: #1b1d21;
}
.accent:hover {
  background: #fff;
}
.accent.on {
  background: var(--accent);
  color: var(--accent-contrast);
}
.accent.on:hover {
  background: var(--accent-hover);
}
.dark {
  background: rgba(255, 255, 255, 0.07);
  color: #e8eaed;
  border: 1px solid rgba(255, 255, 255, 0.07);
}
.dark:hover {
  background: rgba(255, 255, 255, 0.11);
}

.volrow {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 2px 4px;
  color: #cfd2d6;
}
.volicon {
  display: flex;
  border: none;
  background: transparent;
  color: #cfd2d6;
  cursor: pointer;
  padding: 4px;
  border-radius: 6px;
}
.volicon:hover {
  background: rgba(255, 255, 255, 0.08);
  color: #fff;
}
.volicon.muted {
  opacity: 0.45;
}
.volrow input {
  flex: 1;
  accent-color: var(--accent-hover);
}
.volnum {
  width: 28px;
  text-align: right;
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.55);
  font-variant-numeric: tabular-nums;
}

.sep {
  height: 1px;
  background: rgba(255, 255, 255, 0.08);
  margin: 4px 0;
}

/* 显示键盘等操作失败时的提示（不让失败静默） */
.kberr {
  padding: 5px 8px;
  border-radius: 7px;
  background: rgba(232, 90, 90, 0.14);
  font-size: 11px;
  line-height: 1.5;
  color: #ff9c9c;
  word-break: break-all;
}

.row {
  display: flex;
  align-items: center;
  gap: 12px;
  width: 100%;
  padding: 8px 8px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: inherit;
  text-align: left;
  font-size: 13px;
}
.row.link {
  cursor: pointer;
}
.row.link:hover {
  background: rgba(255, 255, 255, 0.06);
}
.ric {
  display: flex;
  color: #cfd2d6;
}
.rtext {
  flex: 1;
  min-width: 0;
}
.rname {
  color: #e8eaed;
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.rsub {
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.42);
  margin-top: 1px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.rsub.conn {
  color: #6fdc8c;
}

.switch {
  flex: none;
  position: relative;
  width: 38px;
  height: 20px;
  border: none;
  border-radius: 99px;
  background: rgba(255, 255, 255, 0.16);
  cursor: pointer;
  transition: background 0.15s ease;
}
.switch.on {
  background: var(--accent);
}
.switch:disabled {
  opacity: 0.5;
  cursor: default;
}
.knob {
  position: absolute;
  top: 3px;
  left: 3px;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: #fff;
  transition: transform 0.15s ease;
}
.switch.on .knob {
  transform: translateX(18px);
}

/* 网络（二级） */
.curconn {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 9px 10px;
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.08);
  color: #cfd2d6;
}
.mini {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  height: 26px;
  padding: 0 10px;
  border-radius: 7px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(255, 255, 255, 0.05);
  color: #d6d8db;
  font-size: 12px;
  cursor: pointer;
}
.mini:hover {
  background: rgba(255, 255, 255, 0.1);
}
.mini:disabled {
  opacity: 0.5;
  cursor: default;
}
.mini.primary {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--accent-contrast);
}
.mini.primary:hover {
  background: var(--accent-hover);
}

.listhead {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 11.5px;
  color: rgba(255, 255, 255, 0.42);
  padding: 2px 4px 0;
}
.refresh {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: rgba(255, 255, 255, 0.55);
  cursor: pointer;
}
.refresh:hover {
  background: rgba(255, 255, 255, 0.08);
  color: #fff;
}

.err {
  font-size: 11.5px;
  color: #ff9c9c;
  padding: 2px 6px;
}
.hint {
  font-size: 12px;
  color: rgba(255, 255, 255, 0.35);
  text-align: center;
  padding: 14px 0;
}
.note {
  font-size: 11px;
  line-height: 1.6;
  color: rgba(255, 255, 255, 0.32);
  padding: 0 6px;
}

.net {
  border-radius: 8px;
}
.net.sel {
  background: rgba(255, 255, 255, 0.04);
  border: 1px solid rgba(255, 255, 255, 0.08);
}
.netrow {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  padding: 8px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: inherit;
  text-align: left;
  font-size: 13px;
  cursor: pointer;
}
.netrow:hover {
  background: rgba(255, 255, 255, 0.06);
}
.netrow .wait {
  flex: none;
  font-size: 11px;
  color: #8fb0ff;
}
.lock {
  flex: none;
  display: flex;
  color: rgba(255, 255, 255, 0.4);
}
.pwdrow {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 2px 8px 8px;
}
.pwdrow input {
  flex: 1;
  min-width: 0;
  height: 28px;
  padding: 0 9px;
  border-radius: 7px;
  border: 1px solid rgba(255, 255, 255, 0.16);
  background: rgba(255, 255, 255, 0.06);
  color: #e8eaed;
  font-size: 12.5px;
  outline: none;
}
.pwdrow input:focus {
  border-color: rgba(144, 176, 255, 0.6);
}

/* 信号格 */
.sig {
  flex: none;
  display: inline-flex;
  align-items: flex-end;
  gap: 1.5px;
  height: 12px;
}
.sig i {
  width: 3px;
  border-radius: 1px;
  background: rgba(255, 255, 255, 0.16);
}
.sig i:nth-child(1) { height: 4px; }
.sig i:nth-child(2) { height: 6px; }
.sig i:nth-child(3) { height: 9px; }
.sig i:nth-child(4) { height: 12px; }
.sig[data-l="1"] i:nth-child(1),
.sig[data-l="2"] i:nth-child(-n + 2),
.sig[data-l="3"] i:nth-child(-n + 3),
.sig[data-l="4"] i:nth-child(-n + 4) {
  background: #e8eaed;
}

/* 手柄焦点环：gp 导航的可见高亮（无此样式时手柄焦点不可见 =「选不了」） */
:deep(.gp-focus),
.gp-focus {
  outline: 2px solid var(--accent, #4a8fe7);
  outline-offset: 1px;
  border-radius: 6px;
}
</style>
