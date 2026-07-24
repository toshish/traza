import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { viteSingleFile } from 'vite-plugin-singlefile';

// The dashboard ships inside the traza-server binary as ONE self-contained
// HTML document (`include_str!`). The singlefile plugin inlines every JS/CSS
// asset so the server never has to route static files.
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
