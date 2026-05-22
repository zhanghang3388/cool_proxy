<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { NCard, NCode, NSpace, NDescriptions, NDescriptionsItem, useMessage } from 'naive-ui'
import { getRuntimeConfig } from '../api'

const cfg = ref<Record<string, any> | null>(null)
const message = useMessage()

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
})
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

    <n-card title="客户端接入示例">
      <n-code language="bash" :code="sample" />
    </n-card>

    <n-card title="说明">
      <p>修改配置请直接编辑后端的 <code>config.yaml</code>，然后重启服务。</p>
      <p>认证文件目录下的 <code>codex-*.json</code> 会自动加载；通过页面上传的文件也会落到这个目录。</p>
    </n-card>
  </n-space>
</template>
