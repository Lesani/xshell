import { defineConfig } from "vitest/config";

// Separate from vite.config.ts so tests do not load the viteStaticCopy plugin.
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
