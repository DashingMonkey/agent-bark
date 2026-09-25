<script setup lang="ts">
import { onMounted, ref } from "vue";
import { isTauri } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { api } from "../api";
import logo from "../assets/logo.png";

// 「关于」页：图标 + 软件名 + 版本号，下面一个「检查更新」按钮。
// 版本号取自 Tauri 的 package_info（= tauri.conf.json 的 version，与安装包、NSIS 同一个
// 口径），既不写死在前端，也不读 package.json——手改一处不会让界面撒谎。
const version = ref("");

// 检查状态机（与 quota-widget 的关于页一致）：
// 空闲 → 检查中（按钮禁用、文案「检查中…」）→ 三种落点之一：
//   发现新版：按钮变主色「下载新版本 vX」，点击跳发布页
//   已是最新：按钮回到「检查更新」
//   检查失败：状态行给出原因（双源都失败才会走到这里）
const checking = ref(false);
const newer = ref(false);
const latest = ref("");
const source = ref("");
const message = ref("");
const failed = ref(false);

/** 应答源的中文名：出问题时用户一眼能看出是从哪个平台拿到的结论 */
const SOURCE_NAMES: Record<string, string> = { gitee: "Gitee", github: "GitHub" };

onMounted(async () => {
  // 直接用浏览器打开 vite 页面调样式时没有 Tauri 环境：版本号留空，页面照常渲染
  if (!isTauri()) return;
  try {
    version.value = await getVersion();
  } catch (e) {
    console.warn("读取版本号失败：", e);
  }
});

async function check() {
  if (checking.value) return;
  // 已发现新版时按钮是「下载新版本」，点击直接跳发布页（返回的是平台发布页，不直接下文件）
  if (newer.value) {
    failed.value = false;
    try {
      await api.openReleasePage();
    } catch (e) {
      failed.value = true;
      message.value = `打开发布页失败：${e}`;
    }
    return;
  }
  checking.value = true;
  failed.value = false;
  message.value = "";
  try {
    const info = await api.checkUpdate();
    // 本地版本以后端为准回填：两边同源，正常情况下与挂载时读到的值一致
    if (info.current) version.value = info.current;
    newer.value = info.newer;
    latest.value = info.latest;
    source.value = SOURCE_NAMES[info.source] ?? info.source;
    message.value = info.newer
      ? `发现新版本 v${info.latest}（当前 v${info.current}），点击上方按钮前往下载`
      : `已是最新版本 ✓（v${info.current}）`;
  } catch (e) {
    newer.value = false;
    source.value = "";
    failed.value = true;
    message.value = `检查失败：${e}`;
  } finally {
    checking.value = false;
  }
}
</script>

<template>
  <div class="about">
    <img class="about-logo" :src="logo" alt="" />
    <div class="about-name">Agent Bark</div>
    <div class="about-version">{{ version ? `v${version}` : "—" }}</div>
    <p class="about-desc hint">AI 编程助手状态监测工具</p>

    <button class="about-check" :class="{ ghost: !newer }" :disabled="checking" @click="check">
      {{ checking ? "检查中…" : newer ? `下载新版本 v${latest}` : "检查更新" }}
    </button>

    <div v-if="message" class="about-msg" :class="{ failed }">{{ message }}</div>
    <div v-if="source" class="about-src">检查源：{{ source }}</div>
  </div>
</template>

<style scoped>
/* 整页居中：图标 / 名称 / 版本号 / 按钮自上而下一条竖轴（对齐 quota-widget 的关于页） */
.about {
  display: flex;
  flex-direction: column;
  align-items: center;
  text-align: center;
  padding: 36px 0 0;
}

/* 图标 PNG 自带透明圆角（半径 56/256），按同比例裁切，与标题栏 logo 一致 */
.about-logo {
  width: 72px;
  height: 72px;
  border-radius: 16px;
  -webkit-user-drag: none;
}

.about-name { font-size: 18px; font-weight: 600; margin-top: 14px; }

.about-version {
  margin-top: 4px;
  font-family: Consolas, monospace;
  font-size: 13px;
  color: var(--muted);
}

.about-desc { max-width: 420px; margin: 14px 0 0; }

.about-check { margin-top: 22px; padding: 7px 22px; }

.about-msg { margin-top: 12px; font-size: 12px; color: var(--muted); line-height: 1.5; }
.about-msg.failed { color: var(--err); }

.about-src { margin-top: 6px; font-size: 12px; color: var(--muted); }
</style>
