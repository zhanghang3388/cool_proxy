import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://localhost:8317',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: false,
    chunkSizeWarningLimit: 800,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (id.includes('node_modules')) {
            if (id.includes('naive-ui') || id.includes('vueuc') || id.includes('css-render')) {
              return 'naive'
            }
            if (id.includes('vue-router') || id.includes('@vue') || id.includes('/vue/')) {
              return 'vue'
            }
            if (id.includes('axios')) {
              return 'axios'
            }
            return 'vendor'
          }
        },
      },
    },
  },
})
