import { defineConfig } from 'vite';

// The build output is committed to `webapp/static/` and embedded into the
// Rust binary at compile time (see `webapp/src/api.rs`), so `cargo run` needs
// no Node toolchain. Asset names are therefore fixed rather than
// content-hashed: hashed names would churn the committed tree on every build
// and cannot be referenced by `include_str!`.
export default defineConfig({
  root: '.',
  base: '/',
  build: {
    outDir: '../static',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: false,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/app[extname]',
      },
    },
  },
  server: {
    port: 5173,
    // `npm run dev` serves the UI with HMR and proxies the API and the SSE
    // stream to a `just serve` backend on :3000.
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:3000',
        changeOrigin: true,
      },
    },
  },
});
