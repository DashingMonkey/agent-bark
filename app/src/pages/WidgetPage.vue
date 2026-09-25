<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import { api, type BarkConfig } from "../api";
import { toast } from "../toast";

// 悬浮窗设置页：开关类全在这一档（启用 / 自动隐藏），**即点即存**——
// 后端 save_config 里的 widget::sync 会随即创建 / 销毁 / 热更新悬浮窗，
// 不设「保存」按钮（按钮保存属于设置页的免打扰时段那类表单）。

const config = ref<BarkConfig | null>(null);
const loadError = ref("");

/** 「自动隐藏」的口径说明：行里放不下，收进感叹号的悬停提示（与 Agents 接入页同一套 .help-mark） */
const AUTO_HIDE_TIP =
  "安静时（没有会话、或所有会话都在思考中 / 执行工具）2 秒后自动隐藏卡片；" +
  "新会话出现时亮一下（任务被接手的回执）；出现「等待确认 / 等待输入 / 任务完成 / 任务失败」时立即弹出，手动中止不弹出";

async function load() {
  loadError.value = "";
  try {
    config.value = await api.getConfig();
  } catch (e) {
    loadError.value = `加载配置失败：${e}`;
  }
}

/**
 * 只落盘，结果用右下角 toast 反馈（与设置页 persist 同一套约定）。
 * save_config 只信我们改的 enabled / auto_hide，widget.x/y/pinned
 * 后端会从当前配置回填——拖动位置 / 右键固定不会被这里的整份保存覆盖。
 */
async function persist(): Promise<boolean> {
  if (!config.value) return false;
  try {
    await api.saveConfig(config.value);
    return true;
  } catch (e) {
    toast(`保存失败：${e}`, "error");
    return false;
  }
}

/** 开关类即时保存：失败回滚勾选态，别让界面撒谎 */
async function toggle(key: "enabled" | "auto_hide", on: boolean) {
  if (!config.value) return;
  const before = config.value.widget[key];
  config.value.widget[key] = on;
  if (!(await persist())) {
    config.value.widget[key] = before;
  }
}

// 悬浮窗右键「关闭悬浮窗」时本页可能正开着：同步勾选态，别让界面撒谎
let unlistenWidgetClosed: (() => void) | null = null;
/** 卸载标志：listen 的 promise 落定前组件可能已经卸载（注册竞态），
 *  resolve 时若已卸载就立即 unlisten，否则这条监听会永久泄漏在窗口里（code-review §2.16） */
let cancelled = false;

onMounted(() => {
  load();
  cancelled = false;
  listen("bark://widget-closed", () => {
    if (config.value) config.value.widget.enabled = false;
  })
    .then((fn) => {
      if (cancelled) {
        fn(); // 注册完成前已卸载：拿到 unlisten 立即解绑，别泄漏
        return;
      }
      unlistenWidgetClosed = fn;
    })
    .catch((e) => console.warn("订阅悬浮窗关闭事件失败：", e));
});

onUnmounted(() => {
  cancelled = true;
  unlistenWidgetClosed?.();
  unlistenWidgetClosed = null;
});
</script>

<template>
  <div v-if="config">
    <div class="between mb-14">
      <h1 class="page-title">悬浮窗</h1>
      <span class="hint">开关即时生效，无需保存</span>
    </div>

    <!-- 与设置页同一套「开关类设置行」（.setting-row，样式在 style.css） -->
    <div class="card">
      <div class="setting-row">
        <div class="setting-text">
          <span class="setting-label">启用</span>
        </div>
        <label class="switch">
          <input
            type="checkbox"
            :checked="config.widget.enabled"
            @change="toggle('enabled', ($event.target as HTMLInputElement).checked)"
          />
          <span class="track"></span>
        </label>
      </div>
      <!-- 自动隐藏只在启用时有意义：总开关关掉时整行置灰、复选框禁用 -->
      <div class="setting-row" :class="{ 'is-disabled': !config.widget.enabled }">
        <div class="setting-text">
          <div class="label-line">
            <span class="setting-label label-text">自动隐藏</span>
            <!-- 口径说明收进悬停提示：感叹号样式与 Agents 接入页的「接入说明」同一套 -->
            <span v-tooltip="AUTO_HIDE_TIP" class="help-mark" role="img" :aria-label="AUTO_HIDE_TIP">
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <circle cx="8" cy="8" r="6.1" />
                <path d="M8 4.9v4" />
                <circle class="dot" cx="8" cy="11.1" r="0.9" />
              </svg>
            </span>
          </div>
        </div>
        <label class="switch">
          <input
            type="checkbox"
            :checked="config.widget.auto_hide"
            :disabled="!config.widget.enabled"
            @change="toggle('auto_hide', ($event.target as HTMLInputElement).checked)"
          />
          <span class="track"></span>
        </label>
      </div>
    </div>
  </div>

  <div v-else-if="loadError" class="empty">
    {{ loadError }}
    <div class="mt-12"><button class="ghost" @click="load">重试</button></div>
  </div>
  <div v-else class="empty">加载配置中…</div>
</template>

<style scoped>
/* 自动隐藏行的置灰：跟「启用」总开关联动 */
.setting-row.is-disabled { opacity: 0.5; }
</style>
