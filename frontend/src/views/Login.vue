<script setup lang="ts">
import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { NCard, NForm, NFormItem, NInput, NButton, useMessage } from 'naive-ui'
import { ping, setAdminToken } from '../api'

const token = ref('')
const loading = ref(false)
const router = useRouter()
const route = useRoute()
const message = useMessage()

async function handleLogin() {
  if (!token.value.trim()) {
    message.warning('请输入 admin token')
    return
  }
  loading.value = true
  try {
    setAdminToken(token.value.trim())
    const ok = await ping()
    if (!ok) {
      message.error('token 不正确，或后端不可达')
      loading.value = false
      return
    }
    message.success('登录成功')
    const redirect = (route.query.redirect as string) || '/accounts'
    router.push(redirect)
  } catch (e) {
    message.error(`登录失败：${(e as Error).message}`)
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div class="login-wrap">
    <n-card title="Cool Proxy 管理面板" style="width: 360px">
      <n-form @keyup.enter="handleLogin">
        <n-form-item label="Admin Token">
          <n-input
            v-model:value="token"
            type="password"
            show-password-on="click"
            placeholder="config.yaml 里的 admin_token"
          />
        </n-form-item>
        <n-button type="primary" block :loading="loading" @click="handleLogin">登录</n-button>
      </n-form>
    </n-card>
  </div>
</template>

<style scoped>
.login-wrap {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
}
</style>
