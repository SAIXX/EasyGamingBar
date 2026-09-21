<script setup lang="ts">
// 游戏介绍卡片小窗：免焦点 + 鼠标穿透（bar 侧 setIgnoreCursorEvents），
// 悬停游戏图标时贴在悬浮条 games-seg 正下方展开，视觉上与悬浮条连为一体。
// 背景优先用 Steam library_hero（库页面同款横幅），非 Steam 游戏回退为
// exe 图标模糊放大 + 暗色渐变；简介经 Rust 拉取商店公开接口（带磁盘缓存）。
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PhysicalPosition } from "@tauri-apps/api/dpi";
import { listen, emit } from "@tauri-apps/api/event";
import { fetchSteamDescription, steamPlaytime } from "../shared/api";
import { t, currentLang } from "../shared/i18n";
import { bindAccent } from "../shared/skins";

interface IntroPayload {
  /** 游戏 id：锁定态点「开始游戏」时交回 bar 侧启动用 */
  id?: string;
  name: string;
  steamAppId?: string;
  iconPath?: string;
  playSeconds?: number;
  lastPlayed?: number;
  launches?: number;
  /** 卡片水平中心的物理坐标（bar 侧按 games-seg 中心换算） */
  x: number;
  /** 卡片顶边的物理坐标（bar 下沿 + 间隙） */
  y: number;
  /** 点击锁定态：可接收鼠标（关穿透）、显示「开始游戏」按钮；悬停预览态为 false */
  locked?: boolean;
}

const show = ref(false);
const data = ref<IntroPayload | null>(null);
const OUT_MS = 220; // 退场动画时长，播完再真隐藏窗口
let outTimer: ReturnType<typeof setTimeout> | null = null;
let safetyTimer: ReturnType<typeof setTimeout> | null = null;
let descToken = 0;

/* ---- 背景图：library_hero → header.jpg → 图标模糊兜底 ---- */
const heroStage = ref<0 | 1 | 2>(0); // 0=hero 1=header 2=兜底

const heroUrl = computed(() => {
  const id = data.value?.steamAppId;
  if (!id) return null;
  const base = `https://cdn.cloudflare.steamstatic.com/steam/apps/${id}`;
  return heroStage.value === 0 ? `${base}/library_hero.jpg` : `${base}/header.jpg`;
});

function onHeroError() {
  heroStage.value = heroStage.value === 0 ? 1 : 2;
}
const heroLoaded = ref(false);

const iconUrl = computed(() =>
  data.value?.iconPath ? convertFileSrc(data.value.iconPath) : null
);

/* ---- Steam 库 logo（带透明通道的标题图）；加载失败回退文字标题 ---- */
const logoFailed = ref(false);
const logoUrl = computed(() => {
  const id = data.value?.steamAppId;
  return id && !logoFailed.value
    ? `https://cdn.cloudflare.steamstatic.com/steam/apps/${id}/library_logo.png`
    : null;
});

/* ---- 简介：Rust 拉取商店接口（磁盘缓存），空串时整行隐藏 ---- */
const desc = ref("");
async function loadDesc(appid?: string) {
  descToken++;
  const token = descToken;
  desc.value = "";
  if (!appid) return;
  try {
    const d = await fetchSteamDescription(appid);
    if (token === descToken) desc.value = d;
  } catch {
    /* 离线/接口失败：无简介展示 */
  }
}

/* ---- 统计文案 ---- */
/* Steam 总时长（官方统计，分钟）；本地跟踪只覆盖经悬浮条启动的会话，
   两者取较大者展示，避免双计也避免低估 */
const steamTotalSecs = ref<number | null>(null);

function fmtPlaytime(s?: number): string {
  const total = Math.max(s ?? 0, steamTotalSecs.value ?? 0);
  if (!total || total <= 0) return t("intro.noRecord");
  const h = total / 3600;
  if (h < 1) return t("intro.minutes", { m: Math.max(1, Math.round(total / 60)) });
  return t("intro.hours", {
    h: h.toLocaleString(currentLang(), { maximumFractionDigits: 1 }),
  });
}
function fmtLast(ts?: number): string {
  if (!ts) return t("intro.never");
  const d = new Date(ts * 1000);
  const now = new Date();
  const day = (x: Date) => x.getFullYear() * 10000 + x.getMonth() * 100 + x.getDate();
  if (day(d) === day(now)) return t("intro.today");
  const yest = new Date(now);
  yest.setDate(now.getDate() - 1);
  if (day(d) === day(yest)) return t("intro.yesterday");
  return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()}`;
}

async function onShow(p: IntroPayload) {
  const win = getCurrentWindow();
  const size = await win.outerSize();
  await win.setPosition(
    new PhysicalPosition(Math.round(p.x - size.width / 2), p.y)
  );
  data.value = p;
  heroStage.value = 0;
  heroLoaded.value = false;
  logoFailed.value = false;
  show.value = true;
  // 退场动画中途再显示：取消挂起的窗口隐藏，避免卡片闪没
  if (outTimer) {
    clearTimeout(outTimer);
    outTimer = null;
  }
  await win.show();
  if (safetyTimer) clearTimeout(safetyTimer);
  // 兜底自动消失（bar 侧 mouseleave 丢失时防常驻）
  safetyTimer = setTimeout(hide, 12_000);
  void loadDesc(p.steamAppId);
  // Steam 游戏拉取官方总时长（本地跟踪只算悬浮条启动的会话，会低估）
  steamTotalSecs.value = null;
  if (p.steamAppId) {
    steamPlaytime([p.steamAppId])
      .then((rows) => {
        if (rows[0]) steamTotalSecs.value = rows[0].minutes * 60;
      })
      .catch(() => {});
  }
}

function hide() {
  if (safetyTimer) {
    clearTimeout(safetyTimer);
    safetyTimer = null;
  }
  if (!show.value) return;
  show.value = false;
  if (outTimer) clearTimeout(outTimer);
  outTimer = setTimeout(() => void getCurrentWindow().hide(), OUT_MS);
}

/** 鼠标进出卡片：告诉 bar 侧「我还在卡片上」，别按移开图标收起 */
function onHover(inside: boolean) {
  void emit("intro://hover", { inside });
}


let offHide: (() => void) | null = null;
let offAccent: (() => void) | null = null;
onMounted(async () => {
  // HUD 括角与描边跟随皮肤强调色
  try {
    const { loadConfig } = await import("../shared/api");
    bindAccent((await loadConfig()).settings);
  } catch {
    /* 忽略 */
  }
  // 最先上报：页面已加载（诊断用，也让 bar 知道可以展示了）
  void emit("intro://boot");
  await listen<IntroPayload>("intro://show", (e) => {
    if (e.payload?.name) void onShow(e.payload);
  });
  offHide = await listen("intro://hide", () => hide());
  // 握手：页面 mount 完成才告诉 bar 可以展示，否则 bar 早先发的 intro://show 会丢失
  await listen("intro://ping", () => void emit("intro://ready"));
  void emit("intro://ready");
  // 强调色跟随皮肤换色（换肤时介绍卡窗口常驻，不重挂载）
  offAccent = await listen("config://updated", async () => {
    try {
      const { loadConfig } = await import("../shared/api");
      bindAccent((await loadConfig()).settings);
    } catch {
      /* 忽略 */
    }
  });
});
onBeforeUnmount(() => {
  offHide?.();
  offAccent?.();
  if (outTimer) clearTimeout(outTimer);
  if (safetyTimer) clearTimeout(safetyTimer);
});
</script>

<template>
  <div
    class="card"
    :class="{ show }"
    @mouseenter="onHover(true)"
    @mouseleave="onHover(false)"
  >
    <!-- 主题色只出现在：四角小圆点、分割线、扫描光、少量文字（AAA 启动器比例：
         70% 暗底 + 20% 封面原色 + 10% 强调色） -->
    <div class="hud-dots">
      <i class="tl"></i><i class="tr"></i><i class="bl"></i><i class="br"></i>
    </div>
    <div class="scan"></div>

    <!-- 背景：完整显示，不裁剪不模糊 -->
    <div class="bg">
      <img
        v-if="heroUrl && heroStage < 2"
        :src="heroUrl"
        :class="{ loaded: heroLoaded }"
        @load="heroLoaded = true"
        @error="onHeroError"
        alt=""
        draggable="false"
      />
      <div
        v-show="heroStage === 2 || !heroUrl"
        class="fallback"
        :style="{
          backgroundImage: iconUrl ? `url('${iconUrl}')` : 'none',
        }"
        :class="{ plain: !iconUrl }"
      ></div>
      <div class="shade"></div>
    </div>

    <!-- 内容：标题区 + 简介 + 底部统计条 -->
    <div class="body">
      <div class="head">
        <img
          v-if="logoUrl"
          :src="logoUrl"
          class="logo"
          @error="logoFailed = true"
          alt=""
          draggable="false"
        />
        <div v-else class="title">{{ data?.name }}</div>
      </div>
      <p v-if="desc" class="desc">{{ desc }}</p>
      <div class="stats">
        <div class="stat">
          <span class="label">{{ t("intro.playTime") }}</span>
          <span class="value">{{ fmtPlaytime(data?.playSeconds) }}</span>
        </div>
        <div class="stat">
          <span class="label">{{ t("intro.lastPlayed") }}</span>
          <span class="value">{{ fmtLast(data?.lastPlayed) }}</span>
        </div>
        <div class="stat">
          <span class="label">{{ t("intro.launches") }}</span>
          <span class="value">{{ t("intro.times", { n: data?.launches ?? 0 }) }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.card {
  position: absolute;
  inset: 0;
  /* 8px = Win11 窗口 DWM 系统圆角半径：CSS 与系统圆角对齐，
     避免 CSS 矩形从系统圆角外冒出（边缘露出的错位缝） */
  border-radius: 8px;
  overflow: hidden;
  background: #0b0e14;
  border: 1px solid rgba(255, 255, 255, 0.14);
  box-shadow: 0 14px 44px rgba(0, 0, 0, 0.55), inset 0 1px 0 rgba(255, 255, 255, 0.08);
  opacity: 0;
  transform: translateY(-10px) scale(0.98);
  transform-origin: top center;
  pointer-events: none;
  transition: opacity 0.2s ease, transform 0.26s cubic-bezier(0.3, 1.25, 0.4, 1);
}
.card.show {
  opacity: 1;
  transform: translateY(0) scale(1);
}

/* 背景：完整显示（contain），深色衬底融合留白 */
.bg {
  position: absolute;
  inset: 0;
  background: #0b0e14;
}
.bg img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: contain;
  object-position: center 38%;
  opacity: 0;
  transition: opacity 0.4s ease;
}
.bg img.loaded {
  opacity: 1;
}
/* 非 Steam 兜底：游戏图标完整呈现（放大但不裁剪不模糊），轻微投影 */
.fallback {
  position: absolute;
  inset: 0;
  background-size: contain;
  background-repeat: no-repeat;
  background-position: center 42%;
  opacity: 0.95;
  filter: drop-shadow(0 10px 26px rgba(0, 0, 0, 0.65));
}
.fallback.plain {
  background: linear-gradient(135deg, #2c3a5e 0%, #1a1d27 70%);
  filter: none;
}

/* 四角小圆点：主题色点缀，无直线 */
.hud-dots i {
  position: absolute;
  width: 4px;
  height: 4px;
  border-radius: 50%;
  background: var(--accent, #4a8fe7);
  box-shadow: 0 0 6px var(--accent, #4a8fe7);
  z-index: 3;
  pointer-events: none;
  opacity: 0.85;
}
.hud-dots .tl { top: 7px; left: 7px; }
.hud-dots .tr { top: 7px; right: 7px; }
.hud-dots .bl { bottom: 7px; left: 7px; }
.hud-dots .br { bottom: 7px; right: 7px; }

/* 扫描光带：低透明度周期下扫，科幻氛围 */
.scan {
  position: absolute;
  inset: 0;
  overflow: hidden;
  pointer-events: none;
  z-index: 2;
}
.scan::before {
  content: "";
  position: absolute;
  left: 0;
  right: 0;
  top: -30%;
  height: 26%;
  background: linear-gradient(
    to bottom,
    transparent,
    color-mix(in srgb, var(--accent, #4a8fe7) 7%, transparent) 55%,
    color-mix(in srgb, var(--accent, #4a8fe7) 5%, transparent) 75%,
    transparent
  );
  /* 只动 transform（走合成层）。原来动画 `top` 是每帧重排 + 重绘整块半透明
     渐变，而且 5.6s 无限循环：卡片窗口是「隐藏」不是销毁，动画会一直跑，
     悬停/点击时与悬浮条抢同一份合成预算 —— 顿挫的一大来源。
     另外卡片没显示时直接暂停，别在后台白烧 GPU。 */
  transform: translateY(0);
  animation: intro-sweep 5.6s linear infinite;
  animation-play-state: paused;
  will-change: transform;
}
/* 只有卡片真正显示时才跑扫描光带 */
.card.show .scan::before {
  animation-play-state: running;
}
/* 位移换算：光带自身高 26%，容器高度的 140% ≈ 自身高度的 538% */
@keyframes intro-sweep {
  from {
    transform: translateY(0);
  }
  to {
    transform: translateY(538%);
  }
}
/* 压暗渐变：顶部轻压保证标题可读，底部深压衔接统计条 */
.shade {
  position: absolute;
  inset: 0;
  background: linear-gradient(
    to bottom,
    rgba(8, 10, 15, 0.42) 0%,
    rgba(8, 10, 15, 0.08) 34%,
    rgba(8, 10, 15, 0.62) 62%,
    rgba(8, 10, 15, 0.94) 100%
  );
}

/* 内容 */
.body {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  justify-content: flex-end;
  padding: 20px 20px 0;
}
.head {
  margin-bottom: auto;
  padding-top: 6px;
}
.logo {
  max-width: 62%;
  max-height: 72px;
  object-fit: contain;
  filter: drop-shadow(0 2px 10px rgba(0, 0, 0, 0.6));
}
.title {
  font-size: 25px;
  font-weight: 700;
  letter-spacing: 0.3px;
  color: #fff;
  text-shadow: 0 2px 12px rgba(0, 0, 0, 0.65);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 88%;
}
.desc {
  margin-bottom: 12px;
  font-size: 12.5px;
  line-height: 1.55;
  color: rgba(235, 238, 245, 0.88);
  text-shadow: 0 1px 6px rgba(0, 0, 0, 0.55);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

/* 底部统计条：Steam 库页同款半透明横带 */
.stats {
  display: flex;
  align-items: stretch;
  gap: 0;
  margin: 0 -20px;
  padding: 9px 12px;
  background: rgba(9, 11, 16, 0.72);
  border-top: 1px solid color-mix(in srgb, var(--accent, #4a8fe7) 38%, transparent);
}
.stat {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 1px;
  padding: 0 10px;
  min-width: 0;
}
.stat + .stat {
  border-left: 1px solid rgba(255, 255, 255, 0.1);
}
/* 锁定态的启动按钮：与悬浮条点亮态同一套主题色 */
.label {
  font-size: 11px;
  color: rgba(255, 255, 255, 0.55);
}
.value {
  font-size: 14px;
  font-weight: 600;
  color: #f2f4f8;
  font-variant-numeric: tabular-nums;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
