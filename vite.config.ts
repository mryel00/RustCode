import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  resolve: {
    alias: {
      // Monaco's package exports hide the individual feature modules we import.
      'monaco-esm': fileURLToPath(new URL('./node_modules/monaco-editor/esm/vs', import.meta.url)),
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/src-tauri/**', '**/target/**', '**/crates/**'] },
  },
  worker: { format: 'es' },
  // Pre-bundling turns Monaco's `?worker` import into a module without a
  // default export, which breaks the dev server.
  optimizeDeps: { exclude: ['monaco-esm'] },
  build: {
    target: 'es2022',
    chunkSizeWarningLimit: 6000,
  },
});
