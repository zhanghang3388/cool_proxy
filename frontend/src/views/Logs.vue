<script setup lang="ts">
import { computed, h, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NTag, NSwitch, NPopconfirm, NPagination, useMessage,
  type DataTableColumns,
} from 'naive-ui'
import { LogEntry, listLogs, clearLogs } from '../api'

const logs = ref<LogEntry[]>([])
const auto = ref(true)
const message = useMessage()
let timer: number | null = null

const page = ref(1)
const pageSize = ref(50)
const total = ref(0)

async function refresh() {
  try {
    const offset = (page.value - 1) * pageSize.value
    const res = await listLogs({ limit: pageSize.value, offset })
    logs.value = res.items
    total.value = res.total
  } catch (e) {
    message.error((e as Error).message)
  }
}

watch([page, pageSize], refresh)

onMounted(async () => {
  await refresh()
  startAuto()
})
onUnmounted(stopAuto)

function startAuto() {
  if (timer) return
  timer = window.setInterval(refresh, 4000)
}
function stopAuto() {
  if (timer) {
    window.clearInterval(timer)
    timer = null
  }
}

function toggleAuto(v: boolean) {
  auto.value = v
  if (v) startAuto()
  else stopAuto()
}

function fmtTime(s: string) {
  try {
    return new Date(s).toLocaleString()
  } catch {
    return s
  }
}

function statusType(s: number): 'success' | 'warning' | 'error' | 'default' {
  if (s >= 200 && s < 300) return 'success'
  if (s >= 400 && s < 500) return 'warning'
  if (s >= 500) return 'error'
  return 'default'
}

const columns = computed<DataTableColumns<LogEntry>>(() => [
  { title: '时间', key: 'at', width: 170, render: (r) => fmtTime(r.at) },
  { title: '方法', key: 'method', width: 70 },
  { title: '路径', key: 'path', minWidth: 200, ellipsis: { tooltip: true } },
  {
    title: '模型',
    key: 'model',
    width: 130,
    ellipsis: { tooltip: true },
    render: (r) => r.model ?? '-',
  },
  {
    title: '账号',
    key: 'account_id',
    width: 200,
    ellipsis: { tooltip: true },
    render: (r) => r.account_id ?? '-',
  },
  {
    title: '状态',
    key: 'status',
    width: 80,
    render: (r) =>
      h(NTag, { type: statusType(r.status), size: 'small' }, { default: () => r.status }),
  },
  {
    title: 'in / out / total',
    key: 'tokens',
    width: 150,
    render: (r) => {
      const i = r.input_tokens ?? '-'
      const o = r.output_tokens ?? '-'
      const t = r.total_tokens ?? '-'
      return `${i} / ${o} / ${t}`
    },
  },
  { title: '耗时(ms)', key: 'duration_ms', width: 100 },
  { title: '尝试', key: 'attempts', width: 70 },
  {
    title: '错误',
    key: 'error',
    minWidth: 180,
    ellipsis: { tooltip: true },
    render: (r) => r.error ?? '-',
  },
])

async function doClear() {
  try {
    await clearLogs()
    page.value = 1
    await refresh()
    message.success('已清空')
  } catch (e) {
    message.error((e as Error).message)
  }
}
</script>

<template>
  <n-card title="请求日志">
    <template #header-extra>
      <n-space align="center">
        <span>自动刷新</span>
        <n-switch :value="auto" @update:value="toggleAuto" size="small" />
        <n-button @click="refresh" size="small">手动刷新</n-button>
        <n-popconfirm @positive-click="doClear">
          <template #trigger>
            <n-button size="small" type="error" ghost>清空</n-button>
          </template>
          确定清空所有日志？
        </n-popconfirm>
      </n-space>
    </template>
    <n-data-table
      :columns="columns"
      :data="logs"
      :row-key="(r: LogEntry) => r.id"
      :scroll-x="1480"
      size="small"
      :bordered="false"
    />
    <div style="margin-top: 12px; display: flex; justify-content: flex-end">
      <n-pagination
        v-model:page="page"
        v-model:page-size="pageSize"
        :item-count="total"
        :page-sizes="[20, 50, 100, 200]"
        show-size-picker
        show-quick-jumper
      />
    </div>
  </n-card>
</template>
