import { defineConfig } from 'vite';
import yaml from '@modyfi/vite-plugin-yaml';

export default defineConfig({
  // Served from https://lovieco.github.io/agent-lock/ on GitHub Pages.
  base: process.env.GITHUB_ACTIONS ? '/agent-lock/' : '/',
  plugins: [yaml()],
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
