import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  // Set VITE_BASE_PATH at build time when deploying under a subpath,
  // e.g.  VITE_BASE_PATH=/myclaw-ui/ npm run build
  base: process.env.VITE_BASE_PATH ?? '/',
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/myclaw': {
        target: 'ws://127.0.0.1:18789',
        ws: true,
      },
    },
  },
})
