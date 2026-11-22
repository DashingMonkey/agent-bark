<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { api, type BarkConfig } from "../api";
import { toast } from "../toast";

const { t } = useI18n();

// 未配置渠道的默认模板：只在用户真正编辑该渠道时才会写入配置
const DEFAULT_WEBHOOK_TEMPLATE = '{"title": "{title}", "body": "{body}", "agent": "{agent}"}';

/** 模板可用占位符的展示串（语言无关，直接从代码传参进词表，避免在消息里转义花括号） */
const TEMPLATE_PLACEHOLDERS = "{title} {body} {agent} {event} {project}";

const config = ref<BarkConfig | null>(null);
const saving = ref(false);
const testing = ref<string | null>(null);
const testResult = reactive<Record<string, { ok: boolean; text: string }>>({});
const loadError = ref("");

// ---- 未保存提示（URL / 模板只在「保存」时落盘；页面被 KeepAlive 保活，切页不丢但也不落盘）----

/** 已落盘配置的快照（加载 / 保存成功时更新），与当前表单比对得出 dirty */
const savedSnapshot = ref("");

/** 有未保存的修改：当前渠道配置与已落盘快照不一致 */
const dirty = computed(
  () => !!config.value && JSON.stringify(config.value.channels) !== savedSnapshot.value,
);

async function load() {
  loadError.value = "";
  try {
    // 只读取，不改配置结构：后端序列化出的 "bark": null / "webhook": null 原样保留，
    // 否则用户一进页面点保存就会把默认对象（含非空 template）落盘。
    config.value = await api.getConfig();
    savedSnapshot.value = JSON.stringify(config.value.channels); // 已落盘基线
  } catch (e) {
    loadError.value = t("common.loadConfigFailed", { error: String(e) });
  }
}

onMounted(load);

async function save() {
  if (!config.value) return;
  saving.value = true;
  try {
    await api.saveConfig({ channels: config.value.channels });
    savedSnapshot.value = JSON.stringify(config.value.channels); // 基线跟上，「未保存」提示消失
    toast(t("common.saved"));
  } catch (e) {
    toast(t("common.saveFailed", { error: String(e) }), "error");
  } finally {
    saving.value = false;
  }
}

async function test(channel: string) {
  testing.value = channel;
  try {
    // 测的是**当前表单里的输入**（含未保存的改动）：null 表示这一项没动、用已落盘配置。
    // 旧实现只传渠道名，后端拿的是落盘的旧地址——新填 URL 没保存就点测试，发去的还是旧地址
    if (channel === "bark") {
      await api.testChannel("bark", config.value?.channels.bark?.url ?? null, null);
    } else {
      await api.testChannel(
        "webhook",
        config.value?.channels.webhook?.url ?? null,
        config.value?.channels.webhook?.template ?? null,
      );
    }
    testResult[channel] = { ok: true, text: t("channels.testSent") };
  } catch (e) {
    testResult[channel] = { ok: false, text: t("channels.testFailed", { error: String(e) }) };
  } finally {
    testing.value = null;
  }
}

/** URL / 模板（等请求要素）一变，上一次的测试结果就过期了：立即清掉，别挂着误导 */
function invalidateTest(channel: string) {
  delete testResult[channel];
}

// 仅在用户与渠道交互（聚焦输入框 / 输入 / 切换开关 / 改选项）时创建对象
function ensureBark(): BarkConfig["channels"]["bark"] {
  if (!config.value) return null;
  if (!config.value.channels.bark) {
    config.value.channels.bark = { enabled: false, url: "" };
  }
  return config.value.channels.bark;
}

function ensureWebhook(): BarkConfig["channels"]["webhook"] {
  if (!config.value) return null;
  if (!config.value.channels.webhook) {
    config.value.channels.webhook = {
      enabled: false,
      url: "",
      method: "POST",
      template: DEFAULT_WEBHOOK_TEMPLATE,
    };
  }
  return config.value.channels.webhook;
}

function setBarkEnabled(on: boolean) {
  const bark = ensureBark();
  if (bark) bark.enabled = on;
}

function setBarkUrl(url: string) {
  const bark = ensureBark();
  if (bark) {
    bark.url = url;
    invalidateTest("bark");
  }
}

function setWebhookEnabled(on: boolean) {
  const webhook = ensureWebhook();
  if (webhook) webhook.enabled = on;
}

function setWebhookUrl(url: string) {
  const webhook = ensureWebhook();
  if (webhook) {
    webhook.url = url;
    invalidateTest("webhook");
  }
}

function setWebhookMethod(method: string) {
  const webhook = ensureWebhook();
  if (webhook) {
    webhook.method = method;
    invalidateTest("webhook"); // 请求方式变了，旧结果同样过期
  }
}

function setWebhookTemplate(template: string) {
  const webhook = ensureWebhook();
  if (webhook) {
    webhook.template = template;
    invalidateTest("webhook");
  }
}
</script>

<template>
  <div v-if="config">
    <div class="page-head">
      <div class="page-head-left"><h1 class="page-title">{{ $t("channels.title") }}</h1></div>
      <div class="page-actions"><button :disabled="saving" @click="save">{{ $t("common.save") }}</button></div>
    </div>
    <div class="stack">
      <!-- 未保存提示：URL / 模板只在「保存」时落盘（切页不丢——页面被 KeepAlive 保活），给个明示 -->
      <div v-if="dirty" class="warn-box">{{ $t("common.unsavedChanges") }}</div>

      <!-- Bark -->
      <div class="card">
        <div class="card-head">
          <div class="card-head-left">
            <span class="card-title">Bark (iOS)</span>
            <span class="hint">{{ $t("channels.barkHint") }}</span>
          </div>
          <div class="row">
            <label class="switch">
              <input type="checkbox" :checked="!!config.channels.bark?.enabled" @change="setBarkEnabled(($event.target as HTMLInputElement).checked)" />
              <span class="track"></span>
            </label>
            <button class="ghost" :disabled="testing === 'bark' || !config.channels.bark?.url" @click="test('bark')">{{ $t("common.test") }}</button>
          </div>
        </div>
        <div class="form-rows">
          <input
            type="text"
            class="w-full"
            placeholder="https://api.day.app/YourKey"
            :value="config.channels.bark?.url ?? ''"
            @focus="ensureBark()"
            @input="setBarkUrl(($event.target as HTMLInputElement).value)"
          />
          <div
            v-if="testResult.bark"
            :class="[testResult.bark.ok ? 'ok-box' : 'warn-box']"
          >{{ testResult.bark.text }}</div>
        </div>
      </div>

      <!-- Webhook 模板 -->
      <div class="card">
        <div class="card-head">
          <div class="card-head-left">
            <span class="card-title">{{ $t("channels.webhookTitle") }}</span>
            <span class="hint">{{ $t("channels.webhookHint") }}</span>
          </div>
          <div class="row">
            <label class="switch">
              <input type="checkbox" :checked="!!config.channels.webhook?.enabled" @change="setWebhookEnabled(($event.target as HTMLInputElement).checked)" />
              <span class="track"></span>
            </label>
            <button class="ghost" :disabled="testing === 'webhook' || !config.channels.webhook?.url" @click="test('webhook')">{{ $t("common.test") }}</button>
          </div>
        </div>
        <div class="form-rows">
          <div class="row">
            <input
              type="text"
              class="flex-2"
              placeholder="https://open.feishu.cn/open-apis/bot/v2/hook/xxx"
              :value="config.channels.webhook?.url ?? ''"
              @focus="ensureWebhook()"
              @input="setWebhookUrl(($event.target as HTMLInputElement).value)"
            />
            <select
              :value="config.channels.webhook?.method ?? 'POST'"
              @focus="ensureWebhook()"
              @change="setWebhookMethod(($event.target as HTMLSelectElement).value)"
            >
              <button><selectedcontent></selectedcontent></button>
              <option>POST</option>
              <option>GET</option>
            </select>
          </div>
          <div class="col">
            <textarea
              rows="3"
              spellcheck="false"
              class="code w-full"
              :placeholder="DEFAULT_WEBHOOK_TEMPLATE"
              :value="config.channels.webhook?.template ?? ''"
              @focus="ensureWebhook()"
              @input="setWebhookTemplate(($event.target as HTMLTextAreaElement).value)"
            ></textarea>
            <div class="hint">{{ $t("channels.templateHint", { placeholders: TEMPLATE_PLACEHOLDERS }) }}</div>
            <div v-if="config.channels.webhook?.method === 'GET'" class="hint">{{ $t("channels.getNoBody") }}</div>
          </div>
          <div
            v-if="testResult.webhook"
            :class="[testResult.webhook.ok ? 'ok-box' : 'warn-box']"
          >{{ testResult.webhook.text }}</div>
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
