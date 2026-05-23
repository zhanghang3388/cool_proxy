<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NStatistic, NGrid, NGi, NSelect,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'
import { UsageBucket, UsageReport, getUsage } from '../api'

const message = useMessage()
const data = ref<UsageReport | null>(null)
const range = ref<string>('all')

const rangeOptions = [
  { label: '全部', value: 'all' },
  { label: '今天', value: 'today' },
  { label: '近 24 小时', value: '24h' },
  { label: '近 7 天', value: '7d' },
  { label: '近 30 天', value: '30d' },
]

function rangeMs(): { from_ms?: number; to_ms?: number } {
  const now = Date.now()
  switch (range.value) {
    case 'today': {
      const d = new Date()
      d.setHours(0, 0, 0, 0)
      return { from_ms: d.getTime(), to_ms: now }
    }
    case '24h':
      return { from_ms: now - 24 * 3600_000, to_ms: now }
    case '7d':
      return { from_ms: now - 7 * 24 * 3600_000, to_ms: now }
    case '30d':
      return { from_ms: now - 30 * 24 * 3600_000, to_ms: now }
    default:
      return {}
  }
}

async function refresh() {
  try {
    data.value = await getUsage(rangeMs())
  } catch (e) {
    message.error((e as Error).message)
  }
}

onMounted(refresh)

const modelColumns = computed<DataTableColumns<UsageBucket>>(() => [
  { title: '模型', key: 'key', minWidth: 180, ellipsis: { tooltip: true } },
  { title: '请求数', key: 'count', width: 100 },
  { title: 'Input', key: 'input_tokens', width: 120 },
  { title: 'Output', key: 'output_tokens', width: 120 },
  { title: 'Total', key: 'total_tokens', width: 130 },
])

const accountColumns = computed<DataTableColumns<UsageBucket>>(() => [
  { title: '账号', key: 'key', minWidth: 220, ellipsis: { tooltip: true } },
  { title: '请求数', key: 'count', width: 100 },
  { title: 'Input', key: 'input_tokens', width: 120 },
  { title: 'Output', key: 'output_tokens', width: 120 },
  { title: 'Total', key: 'total_tokens', width: 130 },
])
</script>

<template>
  <n-space vertical :size="16">
    <n-card>
      <template #header>
        <n-space align="center">
          <span>用量统计</span>
          <n-select
            v-model:value="range"
            :options="rangeOptions"
            size="small"
            style="width: 140px"
            @update:value="refresh"
          />
        </n-space>
      </template>
      <template #header-extra>
        <n-button size="small" @click="refresh">手动刷新</n-button>
      </template>
      <n-grid :cols="4" :x-gap="12" v-if="data">
        <n-gi><n-card><n-statistic label="请求数" :value="data.total_count" /></n-card></n-gi>
        <n-gi><n-card><n-statistic label="Input tokens" :value="data.total_input_tokens" /></n-card></n-gi>
        <n-gi><n-card><n-statistic label="Output tokens" :value="data.total_output_tokens" /></n-card></n-gi>
        <n-gi><n-card><n-statistic label="Total tokens" :value="data.total_total_tokens" /></n-card></n-gi>
      </n-grid>
    </n-card>

    <n-card title="按模型" v-if="data">
      <n-data-table
        :columns="modelColumns"
        :data="data.by_model"
        :row-key="(r: UsageBucket) => r.key"
        :bordered="false"
        size="small"
      />
    </n-card>

    <n-card title="按账号 (Top 200)" v-if="data">
      <n-data-table
        :columns="accountColumns"
        :data="data.by_account"
        :row-key="(r: UsageBucket) => r.key"
        :bordered="false"
        size="small"
      />
    </n-card>
  </n-space>
</template>
