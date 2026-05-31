import axios, { AxiosInstance } from 'axios'

export interface ModelStateView {
  model_key: string
  next_retry_after: string | null
  last_status: number | null
  last_error: string | null
  last_kind: string | null
  transient_fails: number
  quota_backoff_lv: number
}

export interface QuotaWindowView {
  used_percent: number | null
  remaining_percent: number | null
  reset_at: string | null
}

export interface AccountQuotaView {
  five_hour: QuotaWindowView | null
  week: QuotaWindowView | null
  checked_at: string | null
  error: string | null
}

export interface AccountView {
  id: string
  email: string
  account_id: string
  plan: string | null
  enabled: boolean
  expire_at: string | null
  last_refresh_at: string | null
  last_used_at: string | null
  failure_count: number
  cooldown_until: string | null
  last_error: string | null
  total_requests: number
  total_failures: number
  expired: boolean
  proxy_url: string
  proxy_id: string | null
  model_states: ModelStateView[]
  quota: AccountQuotaView
}

export interface AccountListResp {
  total: number
  items: AccountView[]
  limit: number
  offset: number
}

export interface QuotaRefreshItem {
  id: string
  ok: boolean
  quota: AccountQuotaView | null
  error: string | null
}

export interface QuotaRefreshResp {
  items: QuotaRefreshItem[]
}

export interface ProxyEntry {
  id: string
  url: string
  label: string
  created_at: string | null
}

export interface RebalanceResult {
  assigned: number
  skipped_no_proxies: boolean
  failed: string[]
}

export interface ProxyTestResult {
  ok: boolean
  latency_ms: number
  ip: string | null
  country: string | null
  region: string | null
  city: string | null
  isp: string | null
  org: string | null
  asn: string | null
  reverse: string | null
  purity_score: number
  purity_label: string
  purity_reasons: string[]
  error: string | null
}

export interface LogEntry {
  id: number
  at: string
  method: string
  path: string
  account_id: string | null
  model: string | null
  status: number
  duration_ms: number
  attempts: number
  input_tokens: number | null
  output_tokens: number | null
  total_tokens: number | null
  error: string | null
}

export interface UsageBucket {
  key: string
  count: number
  input_tokens: number
  output_tokens: number
  total_tokens: number
}

export interface UsageReport {
  total_count: number
  total_input_tokens: number
  total_output_tokens: number
  total_total_tokens: number
  by_model: UsageBucket[]
  by_account: UsageBucket[]
}

export interface StatsView {
  total_accounts: number
  enabled_accounts: number
  cooling_down: number
  model_cooling_down: number
  expired: number
  total_requests: number
  total_failures: number
}

const TOKEN_KEY = 'cool_proxy_admin_token'

export function getAdminToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}
export function setAdminToken(token: string) {
  localStorage.setItem(TOKEN_KEY, token)
}
export function clearAdminToken() {
  localStorage.removeItem(TOKEN_KEY)
}

function buildClient(): AxiosInstance {
  const c = axios.create({
    baseURL: '/api',
    timeout: 30_000,
  })
  c.interceptors.request.use((cfg) => {
    const t = getAdminToken()
    if (t) cfg.headers.Authorization = `Bearer ${t}`
    return cfg
  })
  return c
}

const http = buildClient()

export async function listAccounts(
  params: { limit?: number; offset?: number; q?: string } = {},
): Promise<AccountListResp> {
  const { data } = await http.get<AccountListResp>('/accounts', { params })
  return data
}

export async function uploadAccounts(files: File[]): Promise<{ imported: string[]; errors: string[] }> {
  const fd = new FormData()
  files.forEach((f) => fd.append('file', f, f.name))
  const { data } = await http.post('/accounts', fd, {
    headers: { 'Content-Type': 'multipart/form-data' },
  })
  return data
}

export async function importAccountsJson(
  payload: { text?: string; token?: unknown; tokens?: unknown[] },
): Promise<{ imported: string[]; errors: string[] }> {
  const { data } = await http.post('/accounts/import', payload)
  return data
}

export async function patchAccount(id: string, payload: { enabled?: boolean }): Promise<void> {
  await http.patch(`/accounts/${encodeURIComponent(id)}`, payload)
}
export async function deleteAccount(id: string): Promise<void> {
  await http.delete(`/accounts/${encodeURIComponent(id)}`)
}
export async function refreshAccount(id: string): Promise<void> {
  await http.post(`/accounts/${encodeURIComponent(id)}/refresh`)
}
export async function refreshAccountQuota(id: string): Promise<QuotaRefreshItem> {
  const { data } = await http.post<QuotaRefreshItem>(`/accounts/${encodeURIComponent(id)}/quota`)
  return data
}
export async function refreshAccountQuotas(ids: string[]): Promise<QuotaRefreshResp> {
  const { data } = await http.post<QuotaRefreshResp>('/accounts/quota/refresh', { ids })
  return data
}
export async function resetCooldown(id: string): Promise<void> {
  await http.post(`/accounts/${encodeURIComponent(id)}/reset-cooldown`)
}
export async function reloadFromDisk(): Promise<{ count: number }> {
  const { data } = await http.post('/accounts/reload')
  return data
}

export async function exportToFiles(): Promise<{ written: number; errors: string[] }> {
  const { data } = await http.post('/accounts/export')
  return data
}

export async function getUsage(
  params: { from_ms?: number; to_ms?: number } = {},
): Promise<UsageReport> {
  const { data } = await http.get<UsageReport>('/usage', { params })
  return data
}

export async function getStats(): Promise<StatsView> {
  const { data } = await http.get<StatsView>('/stats')
  return data
}

export async function getRuntimeConfig(): Promise<Record<string, unknown>> {
  const { data } = await http.get('/config')
  return data
}

export async function listLogs(
  params: { limit?: number; before_id?: number } = {},
): Promise<LogEntry[]> {
  const { data } = await http.get<LogEntry[]>('/logs', { params })
  return data
}

export async function clearLogs(): Promise<void> {
  await http.delete('/logs')
}

export async function listProxies(): Promise<ProxyEntry[]> {
  const { data } = await http.get<ProxyEntry[]>('/proxies')
  return data
}
export async function createProxy(url: string, label: string): Promise<ProxyEntry> {
  const { data } = await http.post<ProxyEntry>('/proxies', { url, label })
  return data
}
export async function updateProxy(id: string, payload: { url?: string; label?: string }): Promise<void> {
  await http.patch(`/proxies/${encodeURIComponent(id)}`, payload)
}
export async function deleteProxy(id: string): Promise<void> {
  await http.delete(`/proxies/${encodeURIComponent(id)}`)
}
export async function rebalanceProxies(only_unassigned: boolean): Promise<RebalanceResult> {
  const { data } = await http.post<RebalanceResult>('/proxies/rebalance', { only_unassigned })
  return data
}
export async function testProxy(id: string): Promise<ProxyTestResult> {
  const { data } = await http.post<ProxyTestResult>(
    `/proxies/${encodeURIComponent(id)}/test`,
    null,
    { timeout: 30_000 },
  )
  return data
}

export async function setAccountProxy(
  id: string,
  payload: { proxy_id?: string; url?: string },
): Promise<void> {
  await http.put(`/accounts/${encodeURIComponent(id)}/proxy`, payload)
}

// ===== Kiro 账号池 =====

export interface KiroUsageView {
  plan_name: string | null
  plan_tier: string | null
  credits_total: number | null
  credits_used: number | null
  credits_remaining: number | null
  bonus_total: number | null
  bonus_used: number | null
  bonus_remaining: number | null
  usage_reset_at: string | null
  bonus_expire_days: number | null
  checked_at: string | null
  error: string | null
}

export interface KiroAccountView {
  id: string
  email: string
  user_id: string | null
  login_provider: string | null
  auth_method: string
  enabled: boolean
  expire_at: string | null
  last_refresh_at: string | null
  last_used_at: string | null
  failure_count: number
  cooldown_until: string | null
  last_error: string | null
  total_requests: number
  total_failures: number
  expired: boolean
  proxy_url: string
  proxy_id: string | null
  status: string | null
  status_reason: string | null
  usage: KiroUsageView
}

export interface KiroAccountListResp {
  total: number
  items: KiroAccountView[]
  limit: number
  offset: number
}

export interface KiroQuotaRefreshItem {
  id: string
  ok: boolean
  usage: KiroUsageView | null
  error: string | null
}

export interface KiroQuotaRefreshResp {
  items: KiroQuotaRefreshItem[]
}

export interface KiroStatsView {
  total_accounts: number
  enabled_accounts: number
  cooling_down: number
  expired: number
  total_requests: number
  total_failures: number
}

export async function listKiroAccounts(
  params: { limit?: number; offset?: number; q?: string } = {},
): Promise<KiroAccountListResp> {
  const { data } = await http.get<KiroAccountListResp>('/kiro/accounts', { params })
  return data
}

export async function uploadKiroAccounts(
  files: File[],
): Promise<{ imported: string[]; errors: string[] }> {
  const fd = new FormData()
  files.forEach((f) => fd.append('file', f, f.name))
  const { data } = await http.post('/kiro/accounts', fd, {
    headers: { 'Content-Type': 'multipart/form-data' },
  })
  return data
}

export async function importKiroAccountsJson(
  payload: { text?: string; token?: unknown; tokens?: unknown[] },
): Promise<{ imported: string[]; errors: string[] }> {
  const { data } = await http.post('/kiro/accounts/import', payload)
  return data
}

export async function patchKiroAccount(id: string, payload: { enabled?: boolean }): Promise<void> {
  await http.patch(`/kiro/accounts/${encodeURIComponent(id)}`, payload)
}
export async function deleteKiroAccount(id: string): Promise<void> {
  await http.delete(`/kiro/accounts/${encodeURIComponent(id)}`)
}
export async function refreshKiroAccount(id: string): Promise<void> {
  await http.post(`/kiro/accounts/${encodeURIComponent(id)}/refresh`)
}
export async function refreshKiroAccountQuota(id: string): Promise<KiroQuotaRefreshItem> {
  const { data } = await http.post<KiroQuotaRefreshItem>(
    `/kiro/accounts/${encodeURIComponent(id)}/quota`,
  )
  return data
}
export async function refreshKiroAccountQuotas(ids: string[]): Promise<KiroQuotaRefreshResp> {
  const { data } = await http.post<KiroQuotaRefreshResp>('/kiro/accounts/quota/refresh', { ids })
  return data
}
export async function resetKiroCooldown(id: string): Promise<void> {
  await http.post(`/kiro/accounts/${encodeURIComponent(id)}/reset-cooldown`)
}
export async function setKiroAccountProxy(
  id: string,
  payload: { proxy_id?: string; url?: string },
): Promise<void> {
  await http.put(`/kiro/accounts/${encodeURIComponent(id)}/proxy`, payload)
}
export async function getKiroStats(): Promise<KiroStatsView> {
  const { data } = await http.get<KiroStatsView>('/kiro/stats')
  return data
}

export async function ping(): Promise<boolean> {
  try {
    await http.get('/stats')
    return true
  } catch {
    return false
  }
}
