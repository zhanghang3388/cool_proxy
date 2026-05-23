<script setup lang="ts">
import { computed, h, onMounted, ref } from 'vue'
import {
  NCard, NDataTable, NSpace, NButton, NTag, NPopconfirm, NModal, NForm, NFormItem, NInput,
  NCheckbox, NAlert, useMessage,
  type DataTableColumns,
} from 'naive-ui'
import {
  ProxyEntry, listProxies, createProxy, updateProxy, deleteProxy,
  rebalanceProxies, listAccounts, AccountView,
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
    width: 180,
    render: (r) =>
      h(NSpace, { size: 4 }, {
        default: () => [
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
  </n-space>
</template>
