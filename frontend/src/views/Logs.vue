<script setup lang="ts">
import { computed, h, onMounted, onUnmounted, ref } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NTag, NSwitch, NPopconfirm, useMessage,
  type DataTableColumns,
} from 'naive-ui'
import { LogEntry, listLogs, clearLogs } from '../api'

const logs = ref<LogEntry[]>([])
const auto = ref(true)
const message = useMessage()
let timer: number | null = null

async function refresh() {
  try {
    logs.value = await listLogs(200)
  } catch (e) {
    message.error((e as Error).message)
  }
}

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
  { title: '路径', key: 'path', minWidth: 240, ellipsis: { tooltip: true } },
  {
    title: '账号',
    key: 'account_id',
    width: 220,
    ellipsis: { tooltip: true },
    render: (r) => r.account_id ?? '-',
  },
  {
    title: '状态',
    key: 'status',
    width: 90,
    render: (r) =>
      h(NTag, { type: statusType(r.status), size: 'small' }, { default: () => r.status }),
  },
  { title: '耗时(ms)', key: 'duration_ms', width: 100 },
  { title: '尝试', key: 'attempts', width: 70 },
  {
    title: '错误',
    key: 'error',
    minWidth: 200,
    ellipsis: { tooltip: true },
    render: (r) => r.error ?? '-',
  },
])

async function doClear() {
  try {
    await clearLogs()
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
      :scroll-x="1200"
      size="small"
      :bordered="false"
    />
  </n-card>
</template>
