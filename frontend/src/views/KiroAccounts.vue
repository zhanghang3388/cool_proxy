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
  KiroAccountView, KiroStatsView, ProxyEntry,
  listKiroAccounts, uploadKiroAccounts, importKiroAccountsJson, deleteKiroAccount,
  refreshKiroAccount, resetKiroCooldown, patchKiroAccount, getKiroStats, listProxies,
  setKiroAccountProxy, refreshKiroAccountQuota, refreshKiroAccountQuotas,
} from '../api'
import type { KiroQuotaRefreshItem } from '../api'

const accounts = ref<KiroAccountView[]>([])
const proxies = ref<ProxyEntry[]>([])
const stats = ref<KiroStatsView | null>(null)
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
let quotaSeq = 0

async function refresh(opts: { autoQuota?: boolean } = {}) {
  try {
    const seq = ++quotaSeq
    const offset = (page.value - 1) * pageSize.value
    const q = search.value.trim() || undefined
    const [list, s, p] = await Promise.all([
      listKiroAccounts({ limit: pageSize.value, offset, q }),
      getKiroStats(),
      listProxies(),
    ])
    accounts.value = list.items
    total.value = list.total
    stats.value = s
    proxies.value = p
    if (opts.autoQuota && list.items.length > 0) {
      await loadPageQuota(list.items.map((a) => a.id), { silent: true, seq })
    }
  } catch (e) {
    message.error(`加载失败：${(e as Error).message}`)
  }
}

watch([page, pageSize], () => refresh({ autoQuota: true }))
watch(search, () => {
  if (searchTimer) window.clearTimeout(searchTimer)
  searchTimer = window.setTimeout(() => {
    page.value = 1
    refresh({ autoQuota: true })
  }, 300)
})

onMounted(async () => {
  loading.value = true
  await refresh({ autoQuota: true })
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

function fmtCredits(used: number | null | undefined, totalCredits: number | null | undefined): string {
  if (totalCredits === null || totalCredits === undefined) return '-'
  const u = used ?? 0
  return `${Math.round(u * 10) / 10} / ${Math.round(totalCredits * 10) / 10}`
}

function creditPercent(remaining: number | null | undefined, totalCredits: number | null | undefined): number {
  if (!totalCredits || totalCredits <= 0) return 0
  const r = remaining ?? 0
  return Math.round((r / totalCredits) * 100)
}

function usageLine(
  label: string,
  used: number | null | undefined,
  totalCredits: number | null | undefined,
  remaining: number | null | undefined,
) {
  if (totalCredits === null || totalCredits === undefined) return null
  const pct = creditPercent(remaining, totalCredits)
  return h('div', { class: 'quota-line', title: `${label} 剩余 ${remaining ?? 0} / ${totalCredits}` }, [
    h('span', { class: 'quota-label' }, label),
    h(NProgress, {
      type: 'line',
      percentage: pct,
      height: 8,
      borderRadius: 2,
      fillBorderRadius: 2,
      showIndicator: false,
      status: pct <= 15 ? 'error' : pct <= 35 ? 'warning' : 'success',
    }),
    h('span', { class: 'quota-percent' }, fmtCredits(used, totalCredits)),
  ])
}

function renderUsage(row: KiroAccountView) {
  const u = row.usage
  if (u?.error) {
    return h(NSpace, { vertical: true, size: 4 }, {
      default: () => [
        h(NTag, { type: 'error', size: 'small', title: u.error ?? '' }, { default: () => '查询失败' }),
        u.checked_at ? h('span', { class: 'quota-muted' }, fmtTime(u.checked_at)) : null,
      ],
    })
  }
  const hasCredits = u?.credits_total !== null && u?.credits_total !== undefined
  const hasBonus = u?.bonus_total !== null && u?.bonus_total !== undefined
  if (!hasCredits && !hasBonus) {
    return h(NTag, { size: 'small' }, { default: () => '未查询' })
  }
  return h(NSpace, { vertical: true, size: 4, class: 'quota-box' }, {
    default: () => [
      usageLine('积分', u.credits_used, u.credits_total, u.credits_remaining),
      usageLine('赠送', u.bonus_used, u.bonus_total, u.bonus_remaining),
      u.checked_at ? h('span', { class: 'quota-muted' }, fmtTime(u.checked_at)) : null,
    ],
  })
}

const columns = computed<DataTableColumns<KiroAccountView>>(() => [
  { title: '邮箱', key: 'email', minWidth: 200, ellipsis: { tooltip: true } },
  {
    title: '登录方式',
    key: 'login_provider',
    width: 110,
    render: (row) => {
      const p = row.login_provider ?? (row.auth_method === 'idc' ? 'IdC' : '-')
      return h(NTag, { size: 'small' }, { default: () => p })
    },
  },
  {
    title: '套餐',
    key: 'plan',
    width: 110,
    ellipsis: { tooltip: true },
    render: (row) => row.usage?.plan_name ?? '-',
  },
  {
    title: '额度',
    key: 'usage',
    width: 240,
    render: renderUsage,
  },
  {
    title: '状态',
    key: 'status',
    width: 150,
    render: (row) => {
      const tags: ReturnType<typeof h>[] = []
      if (!row.enabled) {
        tags.push(h(NTag, { type: 'default', size: 'small' }, { default: () => '禁用' }))
      } else if (row.status === 'banned') {
        tags.push(h(NTag, { type: 'error', size: 'small', title: row.status_reason ?? '' }, { default: () => '封禁' }))
      } else if (row.cooldown_until && new Date(row.cooldown_until) > new Date()) {
        tags.push(h(NTag, { type: 'warning', size: 'small' }, { default: () => '冷却中' }))
      } else if (row.expired) {
        tags.push(h(NTag, { type: 'error', size: 'small' }, { default: () => '已过期' }))
      } else {
        tags.push(h(NTag, { type: 'success', size: 'small' }, { default: () => '可用' }))
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
              await setKiroAccountProxy(row.id, { proxy_id: '' })
            } else {
              await setKiroAccountProxy(row.id, { proxy_id: v })
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
    key: 'reqfail',
    width: 110,
    render: (row) => `${row.total_requests} / ${row.total_failures}`,
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
            await patchKiroAccount(row.id, { enabled: v })
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
          h(NButton, { size: 'small', onClick: () => doRefresh(row.id) }, { default: () => '刷新 token' }),
          h(
            NButton,
            { size: 'small', loading: !!quotaLoading.value[row.id], onClick: () => doRefreshQuota(row.id) },
            { default: () => '刷新额度' },
          ),
          h(NButton, { size: 'small', onClick: () => doResetCooldown(row.id) }, { default: () => '清除冷却' }),
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
    await refreshKiroAccount(id)
    message.success('已刷新')
    await refresh({ autoQuota: true })
  } catch (e) {
    message.error(`刷新失败：${(e as Error).message}`)
  }
}

function applyQuotaResults(items: KiroQuotaRefreshItem[]) {
  const byId = new Map(items.filter((item) => item.usage).map((item) => [item.id, item.usage!]))
  accounts.value = accounts.value.map((account) => {
    const usage = byId.get(account.id)
    return usage ? { ...account, usage } : account
  })
}

async function loadPageQuota(ids: string[], opts: { silent?: boolean; seq?: number } = {}) {
  if (!ids.length) return
  bulkQuotaLoading.value = true
  try {
    const res = await refreshKiroAccountQuotas(ids)
    if (opts.seq !== undefined && opts.seq !== quotaSeq) return
    applyQuotaResults(res.items)
    const failed = res.items.filter((item) => !item.ok).length
    if (!opts.silent) {
      if (failed > 0) {
        message.warning(`已刷新，${failed} 个账号查询失败`)
      } else {
        message.success(`已刷新 ${res.items.length} 个账号额度`)
      }
    }
  } catch (e) {
    if (!opts.silent) {
      message.error(`批量刷新失败：${(e as Error).message}`)
    }
  } finally {
    if (opts.seq === undefined || opts.seq === quotaSeq) {
      bulkQuotaLoading.value = false
    }
  }
}

async function doRefreshQuota(id: string) {
  quotaLoading.value = { ...quotaLoading.value, [id]: true }
  try {
    const res = await refreshKiroAccountQuota(id)
    applyQuotaResults([res])
    if (res.ok) {
      message.success('额度已刷新')
    } else {
      message.warning(`额度查询失败：${res.error ?? 'unknown error'}`)
    }
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
  await loadPageQuota(ids)
}

async function doResetCooldown(id: string) {
  try {
    await resetKiroCooldown(id)
    message.success('已清除冷却')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function doDelete(id: string) {
  try {
    await deleteKiroAccount(id)
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
    const res = await uploadKiroAccounts(files)
    if (res.imported.length) message.success(`导入 ${res.imported.length} 个账号`)
    if (res.errors.length) message.warning(`错误：${res.errors.join('; ')}`)
    await refresh()
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
    const res = await importKiroAccountsJson({ text })
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
      <n-gi><n-card><n-statistic label="已过期" :value="stats.expired" /></n-card></n-gi>
      <n-gi><n-card><n-statistic label="累计请求 / 失败" :value="`${stats.total_requests} / ${stats.total_failures}`" /></n-card></n-gi>
    </n-grid>

    <n-card title="Kiro 账号列表">
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
          <n-button @click="doRefreshPageQuota" :loading="bulkQuotaLoading">刷新本页额度</n-button>
          <n-button @click="() => refresh({ autoQuota: true })" :loading="loading">手动刷新</n-button>
        </n-space>
      </template>
      <n-data-table
        :columns="columns"
        :data="accounts"
        :bordered="false"
        :row-key="(row: KiroAccountView) => row.id"
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
          支持 Kiro 授权 JSON（如 <code>kiro-auth-token.json</code>）或 cockpit 导出的账号对象。<br />
          三种格式：单个对象 / 数组 <code>[{...}]</code> / 一行一个（JSONL）。
        </n-alert>
        <n-input
          v-model:value="pasteText"
          type="textarea"
          :autosize="{ minRows: 12, maxRows: 24 }"
          placeholder='{"accessToken":"...","refreshToken":"...","expiresAt":"2026-...","profileArn":"arn:aws:codewhisperer:us-east-1:...:profile/XXXX"}'
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
  min-width: 200px;
}

.quota-line {
  display: grid;
  grid-template-columns: 32px minmax(110px, 1fr) 64px;
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
