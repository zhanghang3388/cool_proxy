<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  NCard, NSpace, NButton, NTag, NPopconfirm, NStatistic, NGrid, NGi, NSwitch,
  NSelect, NInput, NPagination, NModal, NAlert, NCheckbox, NProgress, NEmpty, NSpin,
  useMessage,
} from 'naive-ui'
import {
  ClaudeAccountView, ClaudeStatsView, ProxyEntry, QuotaWindowView,
  listClaudeAccounts, deleteClaudeAccount, refreshClaudeAccount, resetClaudeCooldown,
  patchClaudeAccount, getClaudeStats, listProxies, setClaudeAccountProxy,
  claudeLoginStart, claudeLoginFinish, rebalanceClaudeProxies,
  refreshClaudeAccountQuota, refreshClaudeAccountQuotas,
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

// 额度查询是手动触发的，按账号记 loading；整页轮询只拉列表（含已缓存额度），不打额度端点。
const quotaLoading = ref<Record<string, boolean>>({})
const bulkQuotaLoading = ref(false)

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

function fmtPercent(n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return '-'
  return `${Math.round(n)}%`
}

const proxyOptions = computed(() => [
  { label: '直连', value: '__direct__' },
  ...proxies.value.map((p) => ({
    label: p.label ? `${p.label} (${p.url})` : p.url,
    value: p.id,
  })),
])

// ===== 状态标签 =====
type StatusInfo = { type: 'success' | 'warning' | 'error' | 'default'; label: string }
function statusOf(row: ClaudeAccountView): StatusInfo {
  if (!row.enabled) return { type: 'default', label: '禁用' }
  if (row.cooldown_until && new Date(row.cooldown_until) > new Date()) {
    return { type: 'warning', label: '冷却中' }
  }
  if (row.expired) return { type: 'error', label: '已过期' }
  return { type: 'success', label: '可用' }
}

// ===== 额度 =====
function windowRemaining(w: QuotaWindowView | null): number | null {
  return w?.remaining_percent ?? null
}
function quotaBarStatus(remaining: number | null): 'default' | 'error' | 'warning' | 'success' {
  if (remaining === null) return 'default'
  if (remaining <= 15) return 'error'
  if (remaining <= 35) return 'warning'
  return 'success'
}
function quotaResetTitle(w: QuotaWindowView | null, label: string): string {
  const remaining = windowRemaining(w)
  const reset = w?.reset_at ? `，重置：${fmtTime(w.reset_at)}` : ''
  return `${label} 剩余 ${fmtPercent(remaining)}${reset}`
}
function hasQuota(row: ClaudeAccountView): boolean {
  return !!(row.quota?.five_hour || row.quota?.week)
}

function proxySelectValue(row: ClaudeAccountView): string {
  if (!row.proxy_url) return '__direct__'
  return row.proxy_id ?? '__custom__'
}
function proxySelectOptions(row: ClaudeAccountView) {
  const opts = [...proxyOptions.value]
  const known = !row.proxy_url || row.proxy_id !== null
  if (!known) opts.push({ label: `自定义 (${row.proxy_url})`, value: '__custom__' })
  return opts
}

// ===== 操作 =====
async function onToggleEnabled(row: ClaudeAccountView, v: boolean) {
  try {
    await patchClaudeAccount(row.id, { enabled: v })
    row.enabled = v
    message.success(v ? '已启用' : '已禁用')
  } catch (e) {
    message.error(`操作失败：${(e as Error).message}`)
  }
}

async function onProxyChange(row: ClaudeAccountView, v: string) {
  if (v === '__custom__') return
  try {
    await setClaudeAccountProxy(row.id, { proxy_id: v === '__direct__' ? '' : v })
    message.success('已更新代理')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

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

function applyQuota(id: string, quota: ClaudeAccountView['quota']) {
  const acc = accounts.value.find((a) => a.id === id)
  if (acc) acc.quota = quota
}

async function doQuota(id: string) {
  quotaLoading.value[id] = true
  try {
    const item = await refreshClaudeAccountQuota(id)
    if (item.quota) applyQuota(id, item.quota)
    if (item.ok) message.success('额度已更新')
    else message.warning(`查询失败：${item.error ?? '未知错误'}`)
  } catch (e) {
    const err = e as { response?: { data?: string }; message: string }
    message.error(`查额度失败：${err.response?.data || err.message}`)
  } finally {
    quotaLoading.value[id] = false
  }
}

async function doQuotaAll() {
  bulkQuotaLoading.value = true
  try {
    const ids = accounts.value.map((a) => a.id)
    const resp = await refreshClaudeAccountQuotas(ids)
    for (const item of resp.items) {
      if (item.quota) applyQuota(item.id, item.quota)
    }
    const okCount = resp.items.filter((i) => i.ok).length
    const failCount = resp.items.length - okCount
    message.success(`额度刷新完成：成功 ${okCount}` + (failCount ? `，失败 ${failCount}` : ''))
  } catch (e) {
    message.error(`批量查额度失败：${(e as Error).message}`)
  } finally {
    bulkQuotaLoading.value = false
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

// ===== 重新分配代理 =====
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
            style="width: 220px"
            size="small"
          />
          <n-button type="primary" @click="openLogin">添加账号（OAuth 登录）</n-button>
          <n-button :loading="bulkQuotaLoading" @click="doQuotaAll">刷新全部额度</n-button>
          <n-button @click="showRebalance = true">重新分配代理</n-button>
          <n-button @click="refresh" :loading="loading">手动刷新</n-button>
        </n-space>
      </template>

      <n-spin :show="loading">
        <n-empty v-if="!accounts.length" description="暂无账号，点击右上角「添加账号」登录" style="padding: 40px 0" />

        <n-grid v-else responsive="screen" cols="1 s:1 m:2 l:3 xl:3" :x-gap="12" :y-gap="12">
          <n-gi v-for="row in accounts" :key="row.id">
            <n-card size="small" :bordered="true" class="acc-card">
              <!-- 头部：邮箱 + 状态 + 启用开关 -->
              <div class="acc-head">
                <div class="acc-title" :title="row.email">
                  <span class="acc-email">{{ row.email || row.id }}</span>
                  <n-tag :type="statusOf(row).type" size="small" round>{{ statusOf(row).label }}</n-tag>
                </div>
                <n-switch
                  :value="row.enabled"
                  size="small"
                  @update:value="(v: boolean) => onToggleEnabled(row, v)"
                />
              </div>
              <div class="acc-org" v-if="row.org_name">{{ row.org_name }}</div>

              <!-- 额度块 -->
              <div class="quota-box">
                <template v-if="row.quota?.error">
                  <n-space :size="6" align="center">
                    <n-tag type="error" size="small" :title="row.quota.error">额度查询失败</n-tag>
                    <span class="muted" v-if="row.quota.checked_at">{{ fmtTime(row.quota.checked_at) }}</span>
                  </n-space>
                </template>
                <template v-else-if="!hasQuota(row)">
                  <n-tag size="small">额度未查询</n-tag>
                </template>
                <template v-else>
                  <div class="quota-line" :title="quotaResetTitle(row.quota.five_hour, '5 小时')">
                    <span class="quota-label">5 小时</span>
                    <n-progress
                      type="line"
                      :percentage="Math.round(windowRemaining(row.quota.five_hour) ?? 0)"
                      :height="8"
                      :border-radius="2"
                      :fill-border-radius="2"
                      :show-indicator="false"
                      :status="quotaBarStatus(windowRemaining(row.quota.five_hour))"
                    />
                    <span class="quota-percent">{{ fmtPercent(windowRemaining(row.quota.five_hour)) }}</span>
                  </div>
                  <div class="quota-line" :title="quotaResetTitle(row.quota.week, '7 天')">
                    <span class="quota-label">7 天</span>
                    <n-progress
                      type="line"
                      :percentage="Math.round(windowRemaining(row.quota.week) ?? 0)"
                      :height="8"
                      :border-radius="2"
                      :fill-border-radius="2"
                      :show-indicator="false"
                      :status="quotaBarStatus(windowRemaining(row.quota.week))"
                    />
                    <span class="quota-percent">{{ fmtPercent(windowRemaining(row.quota.week)) }}</span>
                  </div>
                  <div class="muted quota-checked" v-if="row.quota.checked_at">
                    查询于 {{ fmtTime(row.quota.checked_at) }}
                  </div>
                </template>
              </div>

              <!-- 元信息 -->
              <div class="meta-grid">
                <div class="meta-item"><span class="meta-k">到期</span><span class="meta-v">{{ fmtTime(row.expire_at) }}</span></div>
                <div class="meta-item"><span class="meta-k">最近刷新</span><span class="meta-v">{{ fmtTime(row.last_refresh_at) }}</span></div>
                <div class="meta-item"><span class="meta-k">请求 / 失败</span><span class="meta-v">{{ row.total_requests }} / {{ row.total_failures }}</span></div>
              </div>

              <div class="proxy-row">
                <span class="meta-k">代理</span>
                <n-select
                  :value="proxySelectValue(row)"
                  :options="proxySelectOptions(row)"
                  size="small"
                  :consistent-menu-width="false"
                  @update:value="(v: string) => onProxyChange(row, v)"
                />
              </div>

              <div class="acc-err" v-if="row.last_error" :title="row.last_error">
                最近错误：{{ row.last_error }}
              </div>

              <!-- 操作 -->
              <template #action>
                <n-space :size="6">
                  <n-button size="small" type="primary" ghost :loading="quotaLoading[row.id]" @click="doQuota(row.id)">查额度</n-button>
                  <n-button size="small" @click="doRefresh(row.id)">刷新 token</n-button>
                  <n-button size="small" @click="doResetCooldown(row.id)">清除冷却</n-button>
                  <n-popconfirm @positive-click="() => doDelete(row.id)">
                    <template #trigger>
                      <n-button size="small" type="error" ghost>删除</n-button>
                    </template>
                    确定删除该账号？
                  </n-popconfirm>
                </n-space>
              </template>
            </n-card>
          </n-gi>
        </n-grid>
      </n-spin>

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
          <n-alert type="info" :show-icon="false">
            复制下面的授权链接，自行在浏览器中打开并完成授权。<br />
            授权后浏览器会跳到 <code>http://localhost:54545/callback?code=...</code>（页面打不开是正常的，本机没有监听）。
            <b>直接把地址栏那一整条 URL 复制粘贴到下面即可</b>，也可只粘 <code>code</code> 的值。
          </n-alert>
          <n-input :value="authUrl" readonly type="textarea" :autosize="{ minRows: 2, maxRows: 4 }" spellcheck="false" />
          <n-space>
            <n-button size="small" @click="copyAuthUrl">复制授权链接</n-button>
            <n-button size="small" tag="a" :href="authUrl" target="_blank">在新标签打开</n-button>
          </n-space>
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

<style scoped>
.acc-card {
  height: 100%;
}
.acc-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.acc-title {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.acc-email {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.acc-org {
  margin-top: 2px;
  font-size: 12px;
  color: #909399;
}
.quota-box {
  margin: 12px 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.quota-line {
  display: flex;
  align-items: center;
  gap: 8px;
}
.quota-label {
  width: 42px;
  font-size: 12px;
  color: #606266;
  flex-shrink: 0;
}
.quota-line :deep(.n-progress) {
  flex: 1;
}
.quota-percent {
  width: 40px;
  text-align: right;
  font-size: 12px;
  color: #606266;
  flex-shrink: 0;
}
.quota-checked {
  font-size: 11px;
}
.meta-grid {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 16px;
}
.meta-item {
  display: flex;
  gap: 6px;
  font-size: 12px;
}
.meta-k {
  color: #909399;
  flex-shrink: 0;
}
.meta-v {
  color: #303133;
}
.proxy-row {
  margin-top: 8px;
  display: flex;
  align-items: center;
  gap: 8px;
}
.proxy-row :deep(.n-select) {
  flex: 1;
}
.acc-err {
  margin-top: 8px;
  font-size: 12px;
  color: #d03050;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.muted {
  color: #909399;
}
</style>
