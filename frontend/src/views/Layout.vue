<script setup lang="ts">
import { useRouter, useRoute } from 'vue-router'
import { computed } from 'vue'
import { NLayout, NLayoutHeader, NLayoutSider, NLayoutContent, NMenu, NButton } from 'naive-ui'
import { clearAdminToken } from '../api'

const router = useRouter()
const route = useRoute()

const menu = [
  { label: '账号管理', key: 'accounts' },
  { label: 'Kiro 账号池', key: 'kiro' },
  { label: '代理池', key: 'proxies' },
  { label: '请求日志', key: 'logs' },
  { label: '用量统计', key: 'usage' },
  { label: '设置', key: 'settings' },
]

const activeKey = computed(() => route.name as string)

function onSelect(key: string) {
  router.push({ name: key })
}

function logout() {
  clearAdminToken()
  router.push({ name: 'login' })
}
</script>

<template>
  <n-layout style="height: 100vh">
    <n-layout-header bordered style="padding: 12px 24px; display: flex; justify-content: space-between; align-items: center;">
      <span style="font-weight: 600; font-size: 16px;">Cool Proxy</span>
      <n-button text @click="logout">退出登录</n-button>
    </n-layout-header>
    <n-layout has-sider style="height: calc(100vh - 53px)">
      <n-layout-sider bordered :width="180" :native-scrollbar="false">
        <n-menu
          :value="activeKey"
          :options="menu"
          @update:value="onSelect"
          style="padding-top: 12px"
        />
      </n-layout-sider>
      <n-layout-content :native-scrollbar="false" content-style="padding: 24px">
        <router-view />
      </n-layout-content>
    </n-layout>
  </n-layout>
</template>
