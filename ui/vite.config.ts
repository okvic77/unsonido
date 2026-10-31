import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    // Stable, hash-free filenames: the Rust binary embeds these exact paths
    // (see src/api.rs), so they must not change between builds. The CSS chunk
    // takes its name from the "index" entry, hence the override.
    rollupOptions: {
      output: {
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/app.js",
        assetFileNames: (info) =>
          info.names.some((n) => n.endsWith(".css"))
            ? "assets/app.css"
            : "assets/[name][extname]",
      },
    },
  },
  server: {
    proxy: {
      // In dev, Vite serves the UI and forwards API calls to the Rust server.
      "^/(alerts|namespaces|status|volume|background|airplay|tts)": "http://127.0.0.1:7700",
    },
  },
});