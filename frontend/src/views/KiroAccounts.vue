<script setup lang="ts">
import { computed, h, onMounted, onUnmounted, ref, watch, type VNode } from 'vue'
import {
  NCard, NSpace, NButton, NTag, NUpload, NPopconfirm, NStatistic, NGrid, NGi, NSwitch,
  NSelect, NInput, NPagination, NModal, NAlert, NProgress, NSpin, NEmpty,
  useMessage,
  type UploadFileInfo,
} from 'naive-ui'
import {
  KiroAccountView, KiroStatsView, ProxyEntry,
  listKiroAccounts, uploadKiroAccounts, importKiroAccountsJson, deleteKiroAccount,
  refreshKiroAccount, resetKiroCooldown, patchKiroAccount, getKiroStats, listProxies,
  setKiroAccountProxy, refreshKiroAccountQuota, refreshKiroAccountQuotas,
  kiroSsoLoginStart, kiroSsoLoginFinish,
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

// 把 renderUsage 产出的 vnode 直接渲染到模板（卡片里复用表格时期的额度渲染逻辑）。
const RenderVNode = (props: { vnode: VNode | null }) => props.vnode

function statusInfo(row: KiroAccountView): {
  type: 'default' | 'error' | 'warning' | 'success'
  label: string
  title?: string
} {
  if (!row.enabled) return { type: 'default', label: '禁用' }
  if (row.status === 'banned') return { type: 'error', label: '封禁', title: row.status_reason ?? '' }
  if (row.cooldown_until && new Date(row.cooldown_until) > new Date()) {
    return { type: 'warning', label: '冷却中' }
  }
  if (row.expired) return { type: 'error', label: '已过期' }
  return { type: 'success', label: '可用' }
}

function providerLabel(row: KiroAccountView): string {
  return row.login_provider ?? (row.auth_method === 'idc' ? 'IdC' : '-')
}

function proxyOptionsFor(row: KiroAccountView) {
  const opts = [
    { label: '直连', value: '__direct__' },
    ...proxies.value.map((p) => ({
      label: p.label ? `${p.label} (${p.url})` : p.url,
      value: p.id,
    })),
  ]
  const known = !row.proxy_url || row.proxy_id !== null
  if (!known) opts.push({ label: `自定义 (${row.proxy_url})`, value: '__custom__' })
  return opts
}

function proxyValue(row: KiroAccountView): string {
  return !row.proxy_url ? '__direct__' : (row.proxy_id ?? '__custom__')
}

async function onProxyChange(row: KiroAccountView, v: string) {
  if (v === '__custom__') return
  try {
    await setKiroAccountProxy(row.id, { proxy_id: v === '__direct__' ? '' : v })
    message.success('已更新代理')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function onToggleEnabled(row: KiroAccountView, v: boolean) {
  try {
    await patchKiroAccount(row.id, { enabled: v })
    row.enabled = v
    message.success(v ? '已启用' : '已禁用')
  } catch (e) {
    message.error(`操作失败：${(e as Error).message}`)
  }
}

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

// ===== 企业 SSO（AWS IAM Identity Center / Start URL）登录 =====
const showSso = ref(false)
const ssoStep = ref<1 | 2>(1)
const ssoProxyId = ref('__direct__')
const ssoStartUrl = ref('')
const ssoRegion = ref('us-east-1')
const ssoEmail = ref('')
const ssoAuthUrl = ref('')
const ssoState = ref('')
const ssoCode = ref('')
const ssoBusy = ref(false)

const ssoProxyOptions = computed(() => [
  { label: '直连', value: '__direct__' },
  ...proxies.value.map((p) => ({
    label: p.label ? `${p.label} (${p.url})` : p.url,
    value: p.id,
  })),
])

function openSso() {
  ssoStep.value = 1
  ssoProxyId.value = '__direct__'
  ssoStartUrl.value = ''
  ssoRegion.value = 'us-east-1'
  ssoEmail.value = ''
  ssoAuthUrl.value = ''
  ssoState.value = ''
  ssoCode.value = ''
  showSso.value = true
}

async function startSso() {
  const startUrl = ssoStartUrl.value.trim()
  if (!startUrl.startsWith('https://')) {
    message.warning('请填写以 https:// 开头的 Start URL')
    return
  }
  ssoBusy.value = true
  try {
    const proxyId = ssoProxyId.value === '__direct__' ? '' : ssoProxyId.value
    const res = await kiroSsoLoginStart({
      start_url: startUrl,
      region: ssoRegion.value.trim() || 'us-east-1',
      proxy_id: proxyId,
      email: ssoEmail.value.trim() || undefined,
    })
    ssoAuthUrl.value = res.auth_url
    ssoState.value = res.state
    ssoStep.value = 2
  } catch (e) {
    const err = e as { response?: { data?: string }; message: string }
    message.error(`获取授权链接失败：${err.response?.data || err.message}`)
  } finally {
    ssoBusy.value = false
  }
}

async function finishSso() {
  const code = ssoCode.value.trim()
  if (!code) {
    message.warning('请粘贴授权码或回调 URL')
    return
  }
  ssoBusy.value = true
  try {
    const res = await kiroSsoLoginFinish({ state: ssoState.value, code })
    message.success(`已添加账号 ${res.account.email || res.account.id}`)
    showSso.value = false
    await refresh()
  } catch (e) {
    const err = e as { response?: { data?: string }; message: string }
    message.error(`登录失败：${err.response?.data || err.message}`)
  } finally {
    ssoBusy.value = false
  }
}

async function copySsoUrl() {
  try {
    await navigator.clipboard.writeText(ssoAuthUrl.value)
    message.success('已复制授权链接')
  } catch {
    message.warning('复制失败，请手动选择链接复制')
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
          <n-button @click="openSso">企业 SSO 登录</n-button>
          <n-button @click="doRefreshPageQuota" :loading="bulkQuotaLoading">刷新本页额度</n-button>
          <n-button @click="() => refresh({ autoQuota: true })" :loading="loading">手动刷新</n-button>
        </n-space>
      </template>
      <n-spin :show="loading">
        <n-empty
          v-if="!accounts.length"
          description="暂无账号，点击右上角导入 / 企业 SSO 登录"
          style="padding: 40px 0"
        />
        <n-grid v-else responsive="screen" cols="1 s:1 m:2 l:3 xl:3" :x-gap="12" :y-gap="12">
          <n-gi v-for="row in accounts" :key="row.id">
            <n-card size="small" :bordered="true" class="acc-card">
              <!-- 头部：邮箱 + 登录方式 + 状态 + 启用开关 -->
              <div class="acc-head">
                <div class="acc-title" :title="row.email || row.id">
                  <span class="acc-email">{{ row.email || row.id }}</span>
                  <n-tag size="small" round>{{ providerLabel(row) }}</n-tag>
                  <n-tag :type="statusInfo(row).type" size="small" round :title="statusInfo(row).title">
                    {{ statusInfo(row).label }}
                  </n-tag>
                </div>
                <n-switch
                  :value="row.enabled"
                  size="small"
                  @update:value="(v: boolean) => onToggleEnabled(row, v)"
                />
              </div>
              <div class="acc-org" v-if="row.usage?.plan_name">套餐：{{ row.usage.plan_name }}</div>

              <!-- 额度（复用表格时期的 renderUsage） -->
              <div class="quota-wrap">
                <render-v-node :vnode="renderUsage(row)" />
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
                  :value="proxyValue(row)"
                  :options="proxyOptionsFor(row)"
                  size="small"
                  :consistent-menu-width="false"
                  @update:value="(v: string) => onProxyChange(row, v)"
                />
              </div>

              <div class="acc-err" v-if="row.last_error" :title="row.last_error">
                最近错误：{{ row.last_error }}
              </div>

              <template #action>
                <n-space :size="6">
                  <n-button
                    size="small"
                    type="primary"
                    ghost
                    :loading="!!quotaLoading[row.id]"
                    @click="doRefreshQuota(row.id)"
                  >刷新额度</n-button>
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

    <n-modal
      v-model:show="showSso"
      preset="card"
      title="企业 SSO 登录（IAM Identity Center）"
      style="width: 720px"
    >
      <n-space vertical :size="12">
        <n-alert type="info" :show-icon="false">
          用组织的 SSO Start URL 登录企业 Kiro 账号。第 1 步填 Start URL + 区域获取授权链接；
          第 2 步自行在浏览器打开链接完成组织授权，再把回调地址栏里的内容（含
          <code>code=</code>）整段粘回。
        </n-alert>

        <div>
          <div class="sso-label">代理</div>
          <n-select
            v-model:value="ssoProxyId"
            :options="ssoProxyOptions"
            :disabled="ssoStep === 2"
            size="small"
            consistent-menu-width
          />
        </div>

        <template v-if="ssoStep === 1">
          <div>
            <div class="sso-label">SSO Start URL</div>
            <n-input
              v-model:value="ssoStartUrl"
              placeholder="https://your-org.awsapps.com/start"
              clearable
            />
          </div>
          <div>
            <div class="sso-label">区域</div>
            <n-input v-model:value="ssoRegion" placeholder="us-east-1" clearable />
          </div>
          <div>
            <div class="sso-label">邮箱 / 备注（可选，用于识别与去重）</div>
            <n-input v-model:value="ssoEmail" placeholder="可填该企业账号邮箱" clearable />
          </div>
          <n-space justify="end">
            <n-button @click="showSso = false">取消</n-button>
            <n-button type="primary" :loading="ssoBusy" @click="startSso">获取授权链接</n-button>
          </n-space>
        </template>

        <template v-else>
          <n-alert type="info" :show-icon="false">
            复制下面的授权链接，自行在浏览器打开并完成组织授权。授权后浏览器会跳到一个打不开的
            <code>127.0.0.1</code> 地址——把那条地址（含 <code>code=</code>）整段复制粘到下面即可。
          </n-alert>
          <n-input
            :value="ssoAuthUrl"
            readonly
            type="textarea"
            :autosize="{ minRows: 2, maxRows: 4 }"
          />
          <n-space>
            <n-button size="small" @click="copySsoUrl">复制授权链接</n-button>
            <n-button size="small" tag="a" :href="ssoAuthUrl" target="_blank">在新标签打开</n-button>
          </n-space>
          <n-input
            v-model:value="ssoCode"
            type="textarea"
            :autosize="{ minRows: 2, maxRows: 4 }"
            placeholder="粘贴回调 URL 或 code"
            spellcheck="false"
          />
          <n-space justify="space-between">
            <n-button text @click="ssoStep = 1">← 重新填写</n-button>
            <n-space>
              <n-button @click="showSso = false">取消</n-button>
              <n-button type="primary" :loading="ssoBusy" @click="finishSso">完成登录</n-button>
            </n-space>
          </n-space>
        </template>
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

.sso-label {
  margin-bottom: 6px;
  color: #909399;
  font-size: 13px;
}

/* ===== 账号卡片 ===== */
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
  gap: 6px;
  min-width: 0;
  flex-wrap: wrap;
}
.acc-email {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 100%;
}
.acc-org {
  margin-top: 2px;
  font-size: 12px;
  color: #909399;
}
.quota-wrap {
  margin: 12px 0;
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
</style>
