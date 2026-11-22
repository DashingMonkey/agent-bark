<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { listen } from "@tauri-apps/api/event";
import { api, type BarkConfig } from "../api";
import { toast } from "../toast";

const { t } = useI18n();

// 悬浮窗设置页：启用开关，**即点即存**——
// 后端 save_config 里的 widget::sync 会随即创建 / 销毁悬浮窗，
// 不设「保存」按钮（按钮保存属于设置页的免打扰时段那类表单）。

const config = ref<BarkConfig | null>(null);
const loadError = ref("");

async function load() {
  loadError.value = "";
  try {
    config.value = await api.getConfig();
  } catch (e) {
    loadError.value = t("common.loadConfigFailed", { error: String(e) });
  }
}

/**
 * 只落盘，结果用右下角 toast 反馈（与设置页 persist 同一套约定）。
 * 只提交刚改的字段；位置、固定状态和其他开关由后端保留当前值。
 */
async function persist(key: "enabled"): Promise<boolean> {
  if (!config.value) return false;
  try {
    await api.saveConfig({ widget: { [key]: config.value.widget[key] } });
    return true;
  } catch (e) {
    toast(t("common.saveFailed", { error: String(e) }), "error");
    return false;
  }
}

/** 开关类即时保存：失败回滚勾选态，别让界面撒谎 */
async function toggle(key: "enabled", on: boolean) {
  if (!config.value) return;
  const before = config.value.widget[key];
  config.value.widget[key] = on;
  if (!(await persist(key))) {
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
    <div class="page-head">
      <div class="page-head-left"><h1 class="page-title">{{ $t("widgetPage.title") }}</h1></div>
    </div>

    <div class="stack">
      <!-- 与设置页同一套「开关类设置行」（.setting-row，样式在 style.css） -->
      <div class="card">
        <div class="setting-row">
          <div class="setting-text">
            <span class="setting-label">{{ $t("widgetPage.enable") }}</span>
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
      </div>
    </div>
  </div>

  <div v-else-if="loadError" class="empty">
    {{ loadError }}
    <div class="mt-12"><button class="ghost" @click="load">{{ $t("common.retry") }}</button></div>
  </div>
  <div v-else class="empty">{{ $t("common.loadingConfig") }}</div>
</template>
