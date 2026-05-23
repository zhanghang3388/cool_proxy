import { createRouter, createWebHashHistory, RouteRecordRaw } from 'vue-router'
import { getAdminToken } from '../api'

const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/accounts' },
  {
    path: '/login',
    name: 'login',
    component: () => import('../views/Login.vue'),
  },
  {
    path: '/',
    component: () => import('../views/Layout.vue'),
    meta: { requiresAuth: true },
    children: [
      { path: 'accounts', name: 'accounts', component: () => import('../views/Accounts.vue') },
      { path: 'proxies', name: 'proxies', component: () => import('../views/Proxies.vue') },
      { path: 'logs', name: 'logs', component: () => import('../views/Logs.vue') },
      { path: 'usage', name: 'usage', component: () => import('../views/Usage.vue') },
      { path: 'settings', name: 'settings', component: () => import('../views/Settings.vue') },
    ],
  },
]

const router = createRouter({
  history: createWebHashHistory(),
  routes,
})

router.beforeEach((to) => {
  if (to.meta.requiresAuth && !getAdminToken()) {
    return { name: 'login', query: { redirect: to.fullPath } }
  }
  if (to.name === 'login' && getAdminToken()) {
    return { name: 'accounts' }
  }
})

export default router
