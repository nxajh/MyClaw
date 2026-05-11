import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
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
