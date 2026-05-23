<script setup lang="ts">
import { computed, h, onMounted, ref } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NTag, NPopconfirm, NModal, NForm, NFormItem, NInput,
  NCheckbox, NAlert, NDescriptions, NDescriptionsItem, NProgress, NSpin, NList, NListItem,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'
import {
  ProxyEntry, ProxyTestResult, listProxies, createProxy, updateProxy, deleteProxy,
  rebalanceProxies, listAccounts, AccountView, testProxy,
} from '../api'

const proxies = ref<ProxyEntry[]>([])
const accounts = ref<AccountView[]>([])
const message = useMessage()

const showAdd = ref(false)
const showEdit = ref(false)
const formUrl = ref('')
const formLabel = ref('')
const editingId = ref<string | null>(null)

const showRebalance = ref(false)
const onlyUnassigned = ref(true)

const showTest = ref(false)
const testing = ref(false)
const testingRowId = ref<string | null>(null)
const testTarget = ref<ProxyEntry | null>(null)
const testResult = ref<ProxyTestResult | null>(null)

async function refresh() {
  try {
    const [p, a] = await Promise.all([
      listProxies(),
      // 这里只是为了显示"每个代理被多少号绑定"的概览，最多取 1000 条够用
      listAccounts({ limit: 1000, offset: 0 }),
    ])
    proxies.value = p
    accounts.value = a.items
  } catch (e) {
    message.error((e as Error).message)
  }
}

onMounted(refresh)

const usageById = computed<Record<string, number>>(() => {
  const map: Record<string, number> = {}
  for (const a of accounts.value) {
    if (a.proxy_id) {
      map[a.proxy_id] = (map[a.proxy_id] ?? 0) + 1
    }
  }
  return map
})

const accountsWithoutProxy = computed(
  () => accounts.value.filter((a) => !a.proxy_url).length,
)

function openAdd() {
  formUrl.value = ''
  formLabel.value = ''
  showAdd.value = true
}

async function submitAdd() {
  if (!formUrl.value.trim()) {
    message.warning('请填写代理 URL')
    return
  }
  try {
    await createProxy(formUrl.value.trim(), formLabel.value.trim())
    message.success('已添加')
    showAdd.value = false
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

function openEdit(row: ProxyEntry) {
  editingId.value = row.id
  formUrl.value = row.url
  formLabel.value = row.label ?? ''
  showEdit.value = true
}

async function submitEdit() {
  if (!editingId.value) return
  try {
    await updateProxy(editingId.value, {
      url: formUrl.value.trim(),
      label: formLabel.value.trim(),
    })
    message.success('已更新')
    showEdit.value = false
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function doDelete(row: ProxyEntry) {
  try {
    await deleteProxy(row.id)
    message.success('已删除（已绑定的账号继续使用此代理 URL，不受影响）')
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function doRebalance() {
  try {
    const r = await rebalanceProxies(onlyUnassigned.value)
    if (r.skipped_no_proxies) {
      message.warning('代理池为空，无操作')
    } else {
      message.success(`已分配 ${r.assigned} 个账号`)
      if (r.failed.length) message.error(`失败：${r.failed.join('; ')}`)
    }
    showRebalance.value = false
    await refresh()
  } catch (e) {
    message.error((e as Error).message)
  }
}

async function doTest(row: ProxyEntry) {
  testTarget.value = row
  testResult.value = null
  testing.value = true
  testingRowId.value = row.id
  showTest.value = true
  try {
    const r = await testProxy(row.id)
    testResult.value = r
    if (r.ok) {
      message.success(`可用 · ${r.latency_ms} ms`)
    } else {
      message.error(`不可用：${r.error ?? '未知错误'}`)
    }
  } catch (e) {
    message.error((e as Error).message)
    showTest.value = false
  } finally {
    testing.value = false
    testingRowId.value = null
  }
}

function purityColor(score: number): string {
  if (score >= 80) return '#18a058'
  if (score >= 60) return '#2080f0'
  if (score >= 40) return '#f0a020'
  if (score >= 20) return '#d03050'
  return '#a01020'
}

const columns = computed<DataTableColumns<ProxyEntry>>(() => [
  {
    title: '标签',
    key: 'label',
    width: 160,
    render: (r) => r.label || h('span', { style: 'color: #888;' }, '-'),
  },
  { title: '代理 URL', key: 'url', minWidth: 280, ellipsis: { tooltip: true } },
  {
    title: '已绑定账号数',
    key: 'usage',
    width: 130,
    render: (r) => h(NTag, { size: 'small', type: 'info' }, { default: () => usageById.value[r.id] ?? 0 }),
  },
  {
    title: '操作',
    key: 'actions',
    width: 240,
    render: (r) =>
      h(NSpace, { size: 4 }, {
        default: () => [
          h(
            NButton,
            {
              size: 'small',
              type: 'primary',
              ghost: true,
              loading: testingRowId.value === r.id,
              disabled: testing.value && testingRowId.value !== r.id,
              onClick: () => doTest(r),
            },
            { default: () => '测试' },
          ),
          h(NButton, { size: 'small', onClick: () => openEdit(r) }, { default: () => '编辑' }),
          h(NPopconfirm,
            { onPositiveClick: () => doDelete(r) },
            {
              default: () => '从代理池删除该代理？已绑定的账号会保留这个 URL（视为外部代理）。',
              trigger: () =>
                h(NButton, { size: 'small', type: 'error', ghost: true }, { default: () => '删除' }),
            },
          ),
        ],
      }),
  },
])
</script>

<template>
  <n-space vertical :size="16">
    <n-alert v-if="accountsWithoutProxy > 0 && proxies.length > 0" type="info">
      有 {{ accountsWithoutProxy }} 个账号还没绑定代理。点击"重新分配"可以把代理均匀分配给它们。
    </n-alert>

    <n-card title="代理池">
      <template #header-extra>
        <n-space>
          <n-button type="primary" @click="openAdd">添加代理</n-button>
          <n-button @click="showRebalance = true">重新分配</n-button>
          <n-button @click="refresh">刷新</n-button>
        </n-space>
      </template>
      <n-data-table
        :columns="columns"
        :data="proxies"
        :row-key="(r: ProxyEntry) => r.id"
        :bordered="false"
        size="small"
      />
    </n-card>

    <n-card title="说明">
      <p>新导入的认证文件会按 round-robin 自动绑定代理池里的代理；这个绑定写在认证文件里，跟着账号走。</p>
      <p>"重新分配"：勾选"仅未分配"只会处理还没绑定代理的账号；不勾会把所有账号重新均匀分配（破坏现有绑定）。</p>
      <p>支持 <code>http://</code> / <code>https://</code> / <code>socks5://</code>，可带用户名密码：<code>socks5://user:pass@host:1080</code></p>
    </n-card>

    <n-modal v-model:show="showAdd" preset="card" title="添加代理" style="width: 480px">
      <n-form @keyup.enter="submitAdd">
        <n-form-item label="代理 URL">
          <n-input v-model:value="formUrl" placeholder="socks5://user:pass@host:1080" />
        </n-form-item>
        <n-form-item label="标签（可选）">
          <n-input v-model:value="formLabel" placeholder="日本节点 / 香港 1 等" />
        </n-form-item>
        <n-space justify="end">
          <n-button @click="showAdd = false">取消</n-button>
          <n-button type="primary" @click="submitAdd">添加</n-button>
        </n-space>
      </n-form>
    </n-modal>

    <n-modal v-model:show="showEdit" preset="card" title="编辑代理" style="width: 480px">
      <n-form @keyup.enter="submitEdit">
        <n-form-item label="代理 URL">
          <n-input v-model:value="formUrl" />
        </n-form-item>
        <n-form-item label="标签">
          <n-input v-model:value="formLabel" />
        </n-form-item>
        <n-space justify="end">
          <n-button @click="showEdit = false">取消</n-button>
          <n-button type="primary" @click="submitEdit">保存</n-button>
        </n-space>
      </n-form>
    </n-modal>

    <n-modal v-model:show="showRebalance" preset="card" title="重新分配" style="width: 480px">
      <n-space vertical :size="12">
        <n-checkbox v-model:checked="onlyUnassigned">
          仅给未分配代理的账号分配
        </n-checkbox>
        <n-alert v-if="!onlyUnassigned" type="warning">
          这会把所有账号按 round-robin 重新分配代理，覆盖现有绑定！
        </n-alert>
        <n-space justify="end">
          <n-button @click="showRebalance = false">取消</n-button>
          <n-button type="primary" @click="doRebalance">执行</n-button>
        </n-space>
      </n-space>
    </n-modal>

    <n-modal v-model:show="showTest" preset="card" title="代理测试" style="width: 560px">
      <n-space vertical :size="12">
        <n-descriptions v-if="testTarget" :column="1" size="small" bordered>
          <n-descriptions-item label="标签">
            {{ testTarget.label || '-' }}
          </n-descriptions-item>
          <n-descriptions-item label="代理 URL">
            <code style="word-break: break-all;">{{ testTarget.url }}</code>
          </n-descriptions-item>
        </n-descriptions>

        <div v-if="testing" style="text-align: center; padding: 24px 0;">
          <n-spin size="medium" />
          <div style="margin-top: 12px; color: #888;">正在通过该代理请求 ip-api.com…</div>
        </div>

        <template v-else-if="testResult">
          <n-alert v-if="!testResult.ok" type="error" :title="`不可用 · ${testResult.latency_ms} ms`">
            {{ testResult.error ?? '未知错误' }}
          </n-alert>

          <template v-else>
            <n-alert type="success" :title="`可用 · 延迟 ${testResult.latency_ms} ms`">
              出口 IP：<b>{{ testResult.ip ?? '-' }}</b>
            </n-alert>

            <n-card title="出口节点" size="small">
              <n-descriptions :column="2" size="small" label-placement="left" bordered>
                <n-descriptions-item label="IP">
                  {{ testResult.ip ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="国家">
                  {{ testResult.country ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="地区">
                  {{ testResult.region ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="城市">
                  {{ testResult.city ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="ISP" :span="2">
                  {{ testResult.isp ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="组织" :span="2">
                  {{ testResult.org ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="ASN" :span="2">
                  {{ testResult.asn ?? '-' }}
                </n-descriptions-item>
                <n-descriptions-item label="反向 DNS" :span="2">
                  {{ testResult.reverse || '-' }}
                </n-descriptions-item>
              </n-descriptions>
            </n-card>

            <n-card size="small">
              <template #header>
                纯净度
                <n-tag :color="{ color: purityColor(testResult.purity_score), textColor: '#fff' }" size="small" style="margin-left: 8px;">
                  {{ testResult.purity_label }}
                </n-tag>
              </template>
              <n-progress
                type="line"
                :percentage="testResult.purity_score"
                :color="purityColor(testResult.purity_score)"
                :show-indicator="true"
                indicator-placement="inside"
              />
              <div style="margin-top: 12px;">
                <div style="color: #888; font-size: 12px; margin-bottom: 4px;">评分依据：</div>
                <n-list v-if="testResult.purity_reasons.length" size="small" :show-divider="false">
                  <n-list-item v-for="(r, i) in testResult.purity_reasons" :key="i">
                    {{ r }}
                  </n-list-item>
                </n-list>
                <div v-else style="color: #888; font-size: 12px;">
                  无明显机房特征，未发现扣分项。
                </div>
              </div>
            </n-card>
          </template>
        </template>

        <n-space justify="end">
          <n-button
            :disabled="testing || !testTarget"
            @click="testTarget && doTest(testTarget)"
          >
            重新测试
          </n-button>
          <n-button type="primary" @click="showTest = false">关闭</n-button>
        </n-space>
      </n-space>
    </n-modal>
  </n-space>
</template>
