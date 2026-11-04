import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

/**
 * The plugin ships static files, so the build must be self-contained: relative
 * asset URLs (the host serves the bundle from a path it chooses, not from `/`)
 * and no dependency on a dev server at runtime.
 */
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Deterministic output keeps the bundle digest meaningful: the same source
    // must produce the same bytes, since the digest is what the release
    // signature covers.
    rollupOptions: {
      output: {
        entryFileNames: "assets/[name]-[hash].js",
        chunkFileNames: "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
      },
    },
  },
});
