import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { viteSingleFile } from 'vite-plugin-singlefile';

// The dashboard is a standalone SPA. The singlefile plugin inlines every
// JS/CSS asset into one self-contained dist/index.html, so it can be hosted
// from any static file server with no asset routing.
export default defineConfig({
  plugins: [react(), viteSingleFile()],
  server: {
    // During development the real server provides the API.
    proxy: {
      '/v1': 'http://localhost:8080',
    },
  },
  build: {
    target: 'es2020',
    // Deterministic single-file output; no hashed asset names to track.
    assetsInlineLimit: 100000000,
    chunkSizeWarningLimit: 100000000,
    cssCodeSplit: false,
  },
});
