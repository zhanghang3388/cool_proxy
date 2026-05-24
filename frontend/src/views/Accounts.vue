<script setup lang="ts">
import { computed, h, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NTag, NUpload, NPopconfirm, NStatistic, NGrid, NGi, NSwitch,
  NSelect, NInput, NPagination, NModal, NAlert, NProgress,
  useMessage,
  type DataTableColumns,
  type UploadFileInfo,
} from 'naive-ui'
import {
  AccountView, StatsView, ProxyEntry,
  listAccounts, uploadAccounts, importAccountsJson, deleteAccount, refreshAccount, resetCooldown,
  patchAccount, reloadFromDisk, exportToFiles, getStats, listProxies, setAccountProxy,
  refreshAccountQuota, refreshAccountQuotas,
} from '../api'

const accounts = ref<AccountView[]>([])
const proxies = ref<ProxyEntry[]>([])
const stats = ref<StatsView | null>(null)
const loading = ref(false)
const message = useMessage()

const page = ref(1)
const pageSize = ref(50)
const total = ref(0)
const search = ref('')
const quotaLoading = ref<Record<string, boolean>>({})
const bulkQuotaLoading = ref(false)

let timer: number | null = null
let searchTimer: number | null = null

async function refresh() {
  try {
    const offset = (page.value - 1) * pageSize.value
    const q = search.value.trim() || undefined
    const [list, s, p] = await Promise.all([
      listAccounts({ limit: pageSize.value, offset, q }),
      getStats(),
      listProxies(),
    ])
    accounts.value = list.items
    total.value = list.total
    stats.value = s
    proxies.value = p
  } catch (e) {
    message.error(`加载失败：${(e as Error).message}`)
  }
}

watch([page, pageSize], refresh)
watch(search, () => {
  if (searchTimer) window.clearTimeout(searchTimer)
  searchTimer = window.setTimeout(() => {
    page.value = 1
    refresh()
  }, 300)
})

onMounted(async () => {
  loading.value = true
  await refresh()
  loading.value = false
  timer = window.setInterval(refresh, 8000)
})
onUnmounted(() => {
  if (timer) window.clearInterval(timer)
  if (searchTimer) window.clearTimeout(searchTimer)
})

function fmtTime(s: string | null): string {
  if (!s) return '-'
  try {
    return new Date(s).toLocaleString()
  } catch {
    return s
  }
}

function fmtPercent(n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return '-'
  return `${Math.round(n)}%`
}

function quotaProgress(row: AccountView, key: 'five_hour' | 'week', label: string) {
  const window = row.quota?.[key]
  const remaining = window?.remaining_percent
  const reset = window?.reset_at ? `，重置：${fmtTime(window.reset_at)}` : ''
  const title = `${label} 剩余 ${fmtPercent(remaining)}${reset}`
  return h('div', { class: 'quota-line', title }, [
    h('span', { class: 'quota-label' }, label),
    h(NProgress, {
      type: 'line',
      percentage: Math.round(remaining ?? 0),
      height: 8,
      borderRadius: 2,
      fillBorderRadius: 2,
      showIndicator: false,
      processing: false,
      status: remaining === undefined || remaining === null
        ? 'default'
        : remaining <= 15
          ? 'error'
          : remaining <= 35
            ? 'warning'
            : 'success',
    }),
    h('span', { class: 'quota-percent' }, fmtPercent(remaining)),
  ])
}

function renderQuota(row: AccountView) {
  if (row.quota?.error) {
    return h(NSpace, { vertical: true, size: 4 }, {
      default: () => [
        h(NTag, { type: 'error', size: 'small', title: row.quota.error }, { default: () => '查询失败' }),
        row.quota.checked_at
          ? h('span', { class: 'quota-muted' }, fmtTime(row.quota.checked_at))
          : null,
      ],
    })
  }
  if (!row.quota?.five_hour && !row.quota?.week) {
    return h(NTag, { size: 'small' }, { default: () => '未查询' })
  }
  return h(NSpace, { vertical: true, size: 4, class: 'quota-box' }, {
    default: () => [
      quotaProgress(row, 'five_hour', '5h'),
      quotaProgress(row, 'week', '周'),
      row.quota.checked_at
        ? h('span', { class: 'quota-muted' }, fmtTime(row.quota.checked_at))
        : null,
    ],
  })
}

const columns = computed<DataTableColumns<AccountView>>(() => [
  { title: '邮箱', key: 'email', minWidth: 200, ellipsis: { tooltip: true } },
  {
    title: '套餐',
    key: 'plan',
    width: 90,
    render: (row) => row.plan ?? '-',
  },
  {
    title: '额度',
    key: 'quota',
    width: 230,
    render: renderQuota,
  },
  {
    title: '状态',
    key: 'status',
    width: 180,
    render: (row) => {
      const tags: ReturnType<typeof h>[] = []
      if (!row.enabled) {
        tags.push(h(NTag, { type: 'default', size: 'small' }, { default: () => '禁用' }))
      } else if (row.cooldown_until && new Date(row.cooldown_until) > new Date()) {
        tags.push(h(NTag, { type: 'warning', size: 'small' }, { default: () => '冷却中' }))
      } else if (row.expired) {
        tags.push(h(NTag, { type: 'error', size: 'small' }, { default: () => '已过期' }))
      } else {
        tags.push(h(NTag, { type: 'success', size: 'small' }, { default: () => '可用' }))
      }
      const now = Date.now()
      const cooling = (row.model_states ?? []).filter(
        (s) => s.next_retry_after && new Date(s.next_retry_after).getTime() > now,
      )
      if (cooling.length > 0) {
        const detail = cooling
          .map((s) => `${s.model_key || '(全局)'} · ${s.last_kind ?? '?'} → ${fmtTime(s.next_retry_after)}`)
          .join('\n')
        tags.push(
          h(
            NTag,
            { type: 'warning', size: 'small', title: detail },
            { default: () => `+${cooling.length} model 冷却` },
          ),
        )
      }
      return h(NSpace, { size: 4 }, { default: () => tags })
    },
  },
  {
    title: '到期时间',
    key: 'expire_at',
    width: 170,
    render: (row) => fmtTime(row.expire_at),
  },
  {
    title: '代理',
    key: 'proxy',
    width: 220,
    render: (row) => {
      const opts = [
        { label: '直连', value: '__direct__' },
        ...proxies.value.map((p) => ({
          label: p.label ? `${p.label} (${p.url})` : p.url,
          value: p.id,
        })),
      ]
      const known = !row.proxy_url || row.proxy_id !== null
      if (!known) {
        opts.push({ label: `自定义 (${row.proxy_url})`, value: '__custom__' })
      }
      const value = !row.proxy_url ? '__direct__' : row.proxy_id ?? '__custom__'
      return h(NSelect, {
        value,
        options: opts,
        size: 'small',
        consistentMenuWidth: false,
        'onUpdate:value': async (v: string) => {
          if (v === '__custom__') return
          try {
            if (v === '__direct__') {
              await setAccountProxy(row.id, { proxy_id: '' })
            } else {
              await setAccountProxy(row.id, { proxy_id: v })
            }
            message.success('已更新代理')
            await refresh()
          } catch (e) {
            message.error((e as Error).message)
          }
        },
      })
    },
  },
  {
    title: '最近刷新',
    key: 'last_refresh_at',
    width: 170,
    render: (row) => fmtTime(row.last_refresh_at),
  },
  {
    title: '请求 / 失败',
    key: 'usage',
    width: 110,
    render: (row) => `${row.total_requests} / ${row.total_failures}`,
  },
  {
    title: '失败次数',
    key: 'failure_count',
    width: 90,
  },
  {
    title: '最近错误',
    key: 'last_error',
    minWidth: 200,
    ellipsis: { tooltip: true },
    render: (row) => row.last_error ?? '-',
  },
  {
    title: '启用',
    key: 'enabled',
    width: 80,
    render: (row) =>
      h(NSwitch, {
        value: row.enabled,
        size: 'small',
        'onUpdate:value': async (v: boolean) => {
          try {
            await patchAccount(row.id, { enabled: v })
            row.enabled = v
            message.success(v ? '已启用' : '已禁用')
          } catch (e) {
            message.error(`操作失败：${(e as Error).message}`)
          }
        },
      }),
  },
  {
    title: '操作',
    key: 'actions',
    width: 300,
    render: (row) =>
      h(NSpace, { size: 4 }, {
        default: () => [
          h(
            NButton,
            { size: 'small', onClick: () => doRefresh(row.id) },
            { default: () => '刷新 token' },
          ),
          h(
            NButton,
            {
              size: 'small',
              loading: !!quotaLoading.value[row.id],
              onClick: () => doRefreshQuota(row.id),
            },
            { default: () => '刷新额度' },
          ),
          h(
            NButton,
            { size: 'small', onClick: () => doResetCooldown(row.id) },
            { default: () => '清除冷却' },
          ),
          h(
            NPopconfirm,
            { onPositiveClick: () => doDelete(row.id) },
            {
              default: () => '确定删除该账号？',
              trigger: () =>
                h(NButton, { size: 'small', type: 'error', ghost: true }, { default: () => '删除' }),
            },
          ),
        ],
      }),
  },
])

async function doRefresh(id: string) {
  try {
    await refreshAccount(id)
    message.success('已刷新')
    await refresh()
  } catch (e) {
    message.error(`刷新失败：${(e as Error).message}`)
  }
}

async function doRefreshQuota(id: string) {
  quotaLoading.value = { ...quotaLoading.value, [id]: true }
  try {
    const res = await refreshAccountQuota(id)
    if (res.ok) {
      message.success('额度已刷新')
    } else {
      message.warning(`额度查询失败：${res.error ?? 'unknown error'}`)
    }
    await refresh()
  } catch (e) {
    message.error(`额度查询失败：${(e as Error).message}`)
  } finally {
    const next = { ...quotaLoading.value }
    delete next[id]
    quotaLoading.value = next
  }
}

async function doRefreshPageQuota() {
  const ids = accounts.value.map((a) => a.id)
  if (!ids.length) return
  bulkQuotaLoading.value = true
  try {
    const res = await refreshAccountQuotas(ids)
    const failed = res.items.filter((item) => !item.ok).length
    if (failed > 0) {
      message.warning(`已刷新，${failed} 个账号查询失败`)
    } else {
      message.success(`已刷新 ${res.items.length} 个账号额度`)
    }
    await refresh()
  } catch (e) {
    message.error(`批量刷新失败：${(e as Error).message}`)
  } finally {
    bulkQuotaLoading.value = false
  }
}

async function doResetCooldown(id: string) {
  try {
    await resetCooldown(id)
    message.success('已清除冷却')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function doDelete(id: string) {
  try {
    await deleteAccount(id)
    message.success('已删除')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function handleUpload({ fileList }: { fileList: UploadFileInfo[] }) {
  const files: File[] = fileList
    .map((f) => f.file as File | null)
    .filter((f): f is File => !!f)
  if (!files.length) return
  try {
    const res = await uploadAccounts(files)
    if (res.imported.length) message.success(`导入 ${res.imported.length} 个账号`)
    if (res.errors.length) message.warning(`错误：${res.errors.join('; ')}`)
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function handleReload() {
  try {
    const r = await reloadFromDisk()
    message.success(`已重新加载，共 ${r.count} 个账号`)
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function handleExport() {
  try {
    const r = await exportToFiles()
    let msg = `已导出 ${r.written} 个账号到 auths/`
    if (r.errors.length) msg += `；错误：${r.errors.join('; ')}`
    message.success(msg)
  } catch (e) {
    message.error((e as Error).message)
  }
}

const showPaste = ref(false)
const pasteText = ref('')
const pasting = ref(false)

function openPaste() {
  pasteText.value = ''
  showPaste.value = true
}

async function submitPaste() {
  const text = pasteText.value.trim()
  if (!text) {
    message.warning('请粘贴 JSON')
    return
  }
  pasting.value = true
  try {
    const res = await importAccountsJson({ text })
    if (res.imported.length) message.success(`导入 ${res.imported.length} 个账号`)
    if (res.errors.length) message.warning(`错误：${res.errors.join('; ')}`)
    if (res.imported.length) {
      showPaste.value = false
      await refresh()
    }
  } catch (e) {
    const err = e as { response?: { data?: { errors?: string[] } }; message: string }
    const detail = err.response?.data?.errors?.join('; ')
    message.error(detail || err.message)
  } finally {
    pasting.value = false
  }
}
</script>

<template>
  <n-space vertical :size="16">
    <n-grid :cols="5" :x-gap="12" v-if="stats">
      <n-gi><n-card><n-statistic label="账号总数" :value="stats.total_accounts" /></n-card></n-gi>
      <n-gi><n-card><n-statistic label="启用中" :value="stats.enabled_accounts" /></n-card></n-gi>
      <n-gi><n-card><n-statistic label="账号冷却" :value="stats.cooling_down" /></n-card></n-gi>
      <n-gi><n-card><n-statistic label="model 冷却" :value="stats.model_cooling_down" /></n-card></n-gi>
      <n-gi><n-card><n-statistic label="累计请求 / 失败" :value="`${stats.total_requests} / ${stats.total_failures}`" /></n-card></n-gi>
    </n-grid>

    <n-card title="账号列表">
      <template #header-extra>
        <n-space>
          <n-input
            v-model:value="search"
            placeholder="按邮箱 / id 搜索"
            clearable
            style="width: 240px"
            size="small"
          />
          <n-upload
            multiple
            :show-file-list="false"
            :default-upload="false"
            accept=".json,application/json"
            @update:file-list="(list: UploadFileInfo[]) => handleUpload({ fileList: list })"
          >
            <n-button type="primary">上传认证文件</n-button>
          </n-upload>
          <n-button @click="openPaste">粘贴 JSON</n-button>
          <n-button @click="handleExport">导出到 auths/</n-button>
          <n-button @click="handleReload">从磁盘重新加载</n-button>
          <n-button @click="doRefreshPageQuota" :loading="bulkQuotaLoading">刷新本页额度</n-button>
          <n-button @click="refresh" :loading="loading">手动刷新</n-button>
        </n-space>
      </template>
      <n-data-table
        :columns="columns"
        :data="accounts"
        :bordered="false"
        :row-key="(row: AccountView) => row.id"
        :scroll-x="1850"
        size="small"
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

    <n-modal v-model:show="showPaste" preset="card" title="粘贴 JSON 导入" style="width: 720px">
      <n-space vertical :size="12">
        <n-alert type="info" :show-icon="false">
          支持三种格式：<br />
          1) 单个 JSON 对象 <code>{...}</code><br />
          2) JSON 数组 <code>[{...},{...}]</code><br />
          3) 一行一个 JSON（JSONL）
        </n-alert>
        <n-input
          v-model:value="pasteText"
          type="textarea"
          :autosize="{ minRows: 12, maxRows: 24 }"
          placeholder='{"access_token":"...","refresh_token":"...","email":"a@b.c","account_id":"...","type":"codex","expired":"2026-..."}'
          spellcheck="false"
        />
        <n-space justify="end">
          <n-button @click="showPaste = false">取消</n-button>
          <n-button type="primary" :loading="pasting" @click="submitPaste">导入</n-button>
        </n-space>
      </n-space>
    </n-modal>
  </n-space>
</template>

<style scoped>
.quota-box {
  min-width: 190px;
}

.quota-line {
  display: grid;
  grid-template-columns: 24px minmax(120px, 1fr) 42px;
  align-items: center;
  column-gap: 6px;
}

.quota-label {
  color: #606266;
  font-size: 12px;
  line-height: 1;
}

.quota-muted {
  color: #909399;
  font-size: 12px;
}

.quota-percent {
  color: #303133;
  font-size: 12px;
  text-align: right;
}
</style>
