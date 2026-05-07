import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    port: 9922,
    proxy: {
      '/api': 'http://127.0.0.1:9933'
    }
  }
});
