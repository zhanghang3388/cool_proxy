<script setup lang="ts">
import { computed, h, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NTag, NPopconfirm, NStatistic, NGrid, NGi, NSwitch,
  NSelect, NInput, NPagination, NModal, NAlert, NCheckbox,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'
import {
  ClaudeAccountView, ClaudeStatsView, ProxyEntry,
  listClaudeAccounts, deleteClaudeAccount, refreshClaudeAccount, resetClaudeCooldown,
  patchClaudeAccount, getClaudeStats, listProxies, setClaudeAccountProxy,
  claudeLoginStart, claudeLoginFinish, rebalanceClaudeProxies,
} from '../api'

const accounts = ref<ClaudeAccountView[]>([])
const proxies = ref<ProxyEntry[]>([])
const stats = ref<ClaudeStatsView | null>(null)
const loading = ref(false)
const message = useMessage()

const page = ref(1)
const pageSize = ref(50)
const total = ref(0)
const search = ref('')

let timer: number | null = null
let searchTimer: number | null = null

async function refresh() {
  try {
    const offset = (page.value - 1) * pageSize.value
    const q = search.value.trim() || undefined
    const [list, s, p] = await Promise.all([
      listClaudeAccounts({ limit: pageSize.value, offset, q }),
      getClaudeStats(),
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

watch([page, pageSize], () => refresh())
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

const proxyOptions = computed(() => [
  { label: '直连', value: '__direct__' },
  ...proxies.value.map((p) => ({
    label: p.label ? `${p.label} (${p.url})` : p.url,
    value: p.id,
  })),
])

const columns = computed<DataTableColumns<ClaudeAccountView>>(() => [
  { title: '邮箱', key: 'email', minWidth: 200, ellipsis: { tooltip: true } },
  {
    title: '组织',
    key: 'org_name',
    width: 140,
    ellipsis: { tooltip: true },
    render: (row) => row.org_name ?? '-',
  },
  {
    title: '状态',
    key: 'status',
    width: 110,
    render: (row) => {
      if (!row.enabled) {
        return h(NTag, { type: 'default', size: 'small' }, { default: () => '禁用' })
      }
      if (row.cooldown_until && new Date(row.cooldown_until) > new Date()) {
        return h(NTag, { type: 'warning', size: 'small' }, { default: () => '冷却中' })
      }
      if (row.expired) {
        return h(NTag, { type: 'error', size: 'small' }, { default: () => '已过期' })
      }
      return h(NTag, { type: 'success', size: 'small' }, { default: () => '可用' })
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
      const opts = [...proxyOptions.value]
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
            await setClaudeAccountProxy(row.id, { proxy_id: v === '__direct__' ? '' : v })
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
            await patchClaudeAccount(row.id, { enabled: v })
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
    width: 230,
    render: (row) =>
      h(NSpace, { size: 4 }, {
        default: () => [
          h(NButton, { size: 'small', onClick: () => doRefresh(row.id) }, { default: () => '刷新 token' }),
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
    await refreshClaudeAccount(id)
    message.success('已刷新')
    await refresh()
  } catch (e) {
    const err = e as { response?: { data?: string }; message: string }
    message.error(`刷新失败：${err.response?.data || err.message}`)
  }
}

async function doResetCooldown(id: string) {
  try {
    await resetClaudeCooldown(id)
    message.success('已清除冷却')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function doDelete(id: string) {
  try {
    await deleteClaudeAccount(id)
    message.success('已删除')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

// ===== OAuth 登录 =====
const showLogin = ref(false)
const loginStep = ref<1 | 2>(1)
const loginProxyId = ref('__direct__')
const authUrl = ref('')
const loginState = ref('')
const loginCode = ref('')
const loginBusy = ref(false)

function openLogin() {
  loginStep.value = 1
  loginProxyId.value = '__direct__'
  authUrl.value = ''
  loginState.value = ''
  loginCode.value = ''
  showLogin.value = true
}

async function startLogin() {
  loginBusy.value = true
  try {
    const res = await claudeLoginStart()
    authUrl.value = res.auth_url
    loginState.value = res.state
    loginStep.value = 2
    window.open(res.auth_url, '_blank')
  } catch (e) {
    message.error(`获取授权链接失败：${(e as Error).message}`)
  } finally {
    loginBusy.value = false
  }
}

async function finishLogin() {
  const code = loginCode.value.trim()
  if (!code) {
    message.warning('请粘贴授权码')
    return
  }
  loginBusy.value = true
  try {
    const proxyId = loginProxyId.value === '__direct__' ? '' : loginProxyId.value
    const res = await claudeLoginFinish({ state: loginState.value, code, proxy_id: proxyId })
    message.success(`已添加账号 ${res.account.email || res.account.id}`)
    showLogin.value = false
    await refresh()
  } catch (e) {
    const err = e as { response?: { data?: string }; message: string }
    message.error(`登录失败：${err.response?.data || err.message}`)
  } finally {
    loginBusy.value = false
  }
}

async function copyAuthUrl() {
  try {
    await navigator.clipboard.writeText(authUrl.value)
    message.success('已复制授权链接')
  } catch {
    message.warning('复制失败，请手动选择链接复制')
  }
}

// ===== 重新分配代理（与 codex 一致：从共享代理池 round-robin，可复用）=====
const showRebalance = ref(false)
const onlyUnassigned = ref(true)
const rebalanceBusy = ref(false)

async function doRebalance() {
  rebalanceBusy.value = true
  try {
    const r = await rebalanceClaudeProxies(onlyUnassigned.value)
    if (r.skipped_no_proxies) {
      message.warning('代理池为空，无操作')
    } else {
      message.success(`已分配 ${r.assigned} 个账号` + (r.failed.length ? `，${r.failed.length} 个失败` : ''))
    }
    showRebalance.value = false
    await refresh()
  } catch (e) {
    message.error(`重新分配失败：${(e as Error).message}`)
  } finally {
    rebalanceBusy.value = false
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

    <n-card title="Claude 账号列表">
      <template #header-extra>
        <n-space>
          <n-input
            v-model:value="search"
            placeholder="按邮箱 / id 搜索"
            clearable
            style="width: 240px"
            size="small"
          />
          <n-button type="primary" @click="openLogin">添加账号（OAuth 登录）</n-button>
          <n-button @click="showRebalance = true">重新分配代理</n-button>
          <n-button @click="refresh" :loading="loading">手动刷新</n-button>
        </n-space>
      </template>
      <n-data-table
        :columns="columns"
        :data="accounts"
        :bordered="false"
        :row-key="(row: ClaudeAccountView) => row.id"
        :scroll-x="1600"
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

    <n-modal v-model:show="showLogin" preset="card" title="添加 Claude 账号（OAuth）" style="width: 720px">
      <n-space vertical :size="12">
        <n-alert type="info" :show-icon="false">
          通过 Claude Code 官方 OAuth 登录（PKCE）。第 1 步在浏览器完成授权，第 2 步把页面显示的授权码粘回来。
        </n-alert>

        <div>
          <div style="margin-bottom: 6px; color: #606266; font-size: 13px;">登录 / 后续请求使用的代理</div>
          <n-select
            v-model:value="loginProxyId"
            :options="proxyOptions"
            :disabled="loginStep === 2"
            size="small"
            consistent-menu-width
          />
        </div>

        <template v-if="loginStep === 1">
          <n-space justify="end">
            <n-button @click="showLogin = false">取消</n-button>
            <n-button type="primary" :loading="loginBusy" @click="startLogin">获取授权链接</n-button>
          </n-space>
        </template>

        <template v-else>
          <n-alert type="success" :show-icon="false">
            已在新标签打开授权页。若没弹出，<a :href="authUrl" target="_blank">点此打开</a>，或
            <a href="javascript:void(0)" @click="copyAuthUrl">复制链接</a>。<br />
            授权后浏览器会跳到 <code>http://localhost:54545/callback?code=...</code>（页面打不开是正常的，本机没有监听）。
            <b>直接把地址栏那一整条 URL 复制粘贴到下面即可</b>，也可只粘 <code>code</code> 的值。
          </n-alert>
          <n-input
            v-model:value="loginCode"
            type="textarea"
            :autosize="{ minRows: 2, maxRows: 4 }"
            placeholder="粘贴整段回调 URL，或 code 值"
            spellcheck="false"
          />
          <n-space justify="space-between">
            <n-button text @click="loginStep = 1">← 重新获取链接</n-button>
            <n-space>
              <n-button @click="showLogin = false">取消</n-button>
              <n-button type="primary" :loading="loginBusy" @click="finishLogin">完成登录</n-button>
            </n-space>
          </n-space>
        </template>
      </n-space>
    </n-modal>

    <n-modal v-model:show="showRebalance" preset="card" title="重新分配代理" style="width: 480px">
      <n-space vertical :size="12">
        <n-checkbox v-model:checked="onlyUnassigned">仅给未分配代理的账号分配</n-checkbox>
        <n-alert v-if="!onlyUnassigned" type="warning" :show-icon="false">
          这会把所有 Claude 账号按 round-robin 重新分配代理，覆盖现有绑定！代理可被多个账号 / 多种账号类型共用（不独占）。
        </n-alert>
        <n-space justify="end">
          <n-button @click="showRebalance = false">取消</n-button>
          <n-button type="primary" :loading="rebalanceBusy" @click="doRebalance">确定</n-button>
        </n-space>
      </n-space>
    </n-modal>
  </n-space>
</template>
