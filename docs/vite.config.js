import { defineConfig } from 'vite';

export default defineConfig({
  // Served from https://lovieco.github.io/agent-lock/ on GitHub Pages.
  base: process.env.GITHUB_ACTIONS ? '/agent-lock/' : '/',
  server: {
    port: 5173,
    strictPort: false,
    host: '127.0.0.1',
  },
  build: {
    target: 'es2020',
    cssMinify: 'lightningcss',
  },
});
