import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

/**
 * The dev server proxies the kernel's API, so `pnpm dev` talks to a real kernel
 * on the same machine without a token in the URL. Set AGI_API to point it at a
 * kernel that is not on the default port.
 */
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5273,
    proxy: {
      '/api': {
        target: process.env.AGI_API ?? 'http://127.0.0.1:19392',
        ws: true,
        changeOrigin: true,
      },
    },
  },
  build: {
    // The kernel embeds this directory; a single chunk keeps the embed simple.
    outDir: 'dist',
    emptyOutDir: true,
  },
})
