<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import {
  NCard, NCode, NSpace, NDescriptions, NDescriptionsItem, NForm, NFormItem,
  NSwitch, NInputNumber, NButton, NText, useMessage,
} from 'naive-ui'
import {
  getRuntimeConfig, getKiroSettings, updateKiroSettings, resetKiroSettings,
} from '../api'
import type { KiroSettings } from '../api'

const cfg = ref<Record<string, any> | null>(null)
const message = useMessage()

// ===== Kiro 运行期配置表单 =====
const kiro = ref<KiroSettings>({
  compact: true,
  compact_threshold_tokens: 120000,
  tool_result_max_tokens: 4000,
  keep_recent_turns: 8,
  synth_cache: true,
  filter_claude_code: false,
  strip_boundaries: true,
  env_noise: true,
})
const kiroLoaded = ref(false)
const saving = ref(false)
const resetting = ref(false)

const sample = computed(() => {
  const host = (cfg.value?.host as string) || 'localhost'
  const port = (cfg.value?.port as number) || 8317
  return [
    '# OpenAI 兼容',
    `export OPENAI_BASE_URL=http://${host}:${port}/v1`,
    'export OPENAI_API_KEY=<config 中的 api_keys 任意一个>',
    '',
    '# 测试',
    'curl $OPENAI_BASE_URL/chat/completions \\',
    '  -H "Authorization: Bearer $OPENAI_API_KEY" \\',
    '  -H "Content-Type: application/json" \\',
    '  -d \'{"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}\'',
  ].join('\n')
})

onMounted(async () => {
  try {
    cfg.value = await getRuntimeConfig()
  } catch (e) {
    message.error((e as Error).message)
  }
  try {
    kiro.value = await getKiroSettings()
    kiroLoaded.value = true
  } catch (e) {
    message.error('加载 Kiro 配置失败：' + (e as Error).message)
  }
})

async function saveKiro() {
  saving.value = true
  try {
    kiro.value = await updateKiroSettings(kiro.value)
    message.success('已保存，立即生效（无需重启）')
  } catch (e) {
    message.error((e as Error).message)
  } finally {
    saving.value = false
  }
}

async function resetKiro() {
  resetting.value = true
  try {
    kiro.value = await resetKiroSettings()
    message.success('已恢复为 config.yaml 的默认值')
  } catch (e) {
    message.error((e as Error).message)
  } finally {
    resetting.value = false
  }
}
</script>

<template>
  <n-space vertical :size="16">
    <n-card title="运行时配置">
      <n-descriptions v-if="cfg" :column="2" bordered label-placement="left">
        <n-descriptions-item label="监听地址">{{ cfg.host }}:{{ cfg.port }}</n-descriptions-item>
        <n-descriptions-item label="认证文件目录">{{ cfg.auth_dir }}</n-descriptions-item>
        <n-descriptions-item label="上游地址">{{ cfg.upstream?.base_url }}</n-descriptions-item>
        <n-descriptions-item label="可用 API key 数量">{{ cfg.api_keys_count }}</n-descriptions-item>
        <n-descriptions-item label="最大重试">{{ cfg.retry?.max_retries }}</n-descriptions-item>
        <n-descriptions-item label="单次冷却 (秒)">{{ cfg.retry?.cooldown_seconds }}</n-descriptions-item>
        <n-descriptions-item label="长冷却 (秒)">{{ cfg.retry?.long_cooldown_seconds }}</n-descriptions-item>
        <n-descriptions-item label="冷却阈值">{{ cfg.retry?.failure_threshold }}</n-descriptions-item>
        <n-descriptions-item label="刷新扫描间隔 (秒)">{{ cfg.token_refresh?.scan_interval_seconds }}</n-descriptions-item>
        <n-descriptions-item label="提前刷新窗口 (秒)">{{ cfg.token_refresh?.refresh_before_expire_seconds }}</n-descriptions-item>
      </n-descriptions>
    </n-card>

    <n-card title="Kiro 设置（改完即时生效，存数据库，重启不丢）">
      <n-form
        v-if="kiroLoaded"
        label-placement="left"
        label-width="180"
        :show-feedback="false"
      >
        <n-form-item label="透明上下文压缩">
          <n-space vertical :size="2">
            <n-switch v-model:value="kiro.compact" />
            <n-text depth="3" style="font-size: 12px">
              超过阈值时自动截断超大 tool_result + 丢最旧历史，压回 Kiro 上限内，避免「数据过长」被拒（有损）。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="压缩触发阈值 (tokens)">
          <n-space vertical :size="2">
            <n-input-number
              v-model:value="kiro.compact_threshold_tokens"
              :min="1000"
              :step="10000"
              style="width: 220px"
            />
            <n-text depth="3" style="font-size: 12px">
              约 3 字节/token 估算。设为略低于 Kiro 实测上限；可看后端日志 “kiro 输入体量估算 est_tokens=…” 标定。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="单 tool_result 上限 (tokens)">
          <n-space vertical :size="2">
            <n-input-number
              v-model:value="kiro.tool_result_max_tokens"
              :min="100"
              :step="500"
              style="width: 220px"
            />
            <n-text depth="3" style="font-size: 12px">
              单个工具结果（如读大文件）超过则截断保留头部。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="保留最近轮次">
          <n-space vertical :size="2">
            <n-input-number
              v-model:value="kiro.keep_recent_turns"
              :min="1"
              :step="1"
              style="width: 220px"
            />
            <n-text depth="3" style="font-size: 12px">
              压缩丢历史时至少保留最近多少轮（1 轮≈user+assistant 两条）。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="合成 prompt-cache 计费">
          <n-space vertical :size="2">
            <n-switch v-model:value="kiro.synth_cache" />
            <n-text depth="3" style="font-size: 12px">
              把上游真实 input 拆成 cache_read / cache_creation / fresh 写入 usage，让下游像 claude 渠道一样有缓存读写。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="替换 CC 系统提示">
          <n-space vertical :size="2">
            <n-switch v-model:value="kiro.filter_claude_code" />
            <n-text depth="3" style="font-size: 12px">
              检测到 Claude Code 系统提示时整体替换成精简后端提示（较激进，仍命中身份类 403 时再开）。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="去边界标记">
          <n-space vertical :size="2">
            <n-switch v-model:value="kiro.strip_boundaries" />
            <n-text depth="3" style="font-size: 12px">
              去掉 “--- SYSTEM PROMPT ---” 之类边界标记。安全，建议开。
            </n-text>
          </n-space>
        </n-form-item>

        <n-form-item label="去环境/身份噪音">
          <n-space vertical :size="2">
            <n-switch v-model:value="kiro.env_noise" />
            <n-text depth="3" style="font-size: 12px">
              去掉 gitStatus / Recent commits / “you are claude code” / # Environment 段等噪音行。
            </n-text>
          </n-space>
        </n-form-item>

        <n-space justify="end" :size="12" style="margin-top: 8px">
          <n-button :loading="resetting" @click="resetKiro">恢复文件默认</n-button>
          <n-button type="primary" :loading="saving" @click="saveKiro">保存</n-button>
        </n-space>
      </n-form>
    </n-card>

    <n-card title="客户端接入示例">
      <n-code language="bash" :code="sample" />
    </n-card>

    <n-card title="说明">
      <p>上面的 <strong>Kiro 设置</strong> 改完即时生效、存数据库（重启不丢），其值优先于 <code>config.yaml</code>；点「恢复文件默认」可清除覆盖、回到 <code>config.yaml</code> 的 kiro 配置。</p>
      <p>其余配置（监听地址 / 重试 / 刷新等）仍需编辑后端的 <code>config.yaml</code> 再重启服务。</p>
      <p>认证文件目录下的 <code>codex-*.json</code> 会自动加载；通过页面上传的文件也会落到这个目录。</p>
    </n-card>
  </n-space>
</template>
