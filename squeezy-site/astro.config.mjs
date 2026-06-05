import { defineConfig } from "astro/config";

export default defineConfig({
  site: "https://squeezyagent.com",
  output: "static",
  // Preserve source whitespace so the monospace "Quiet Rail" terminal
  // recreations keep their column alignment (white-space: pre).
  compressHTML: false,
  redirects: {
    "/cost": "/how-it-works/",
    "/languages": "/support/#languages",
    "/providers": "/support/#providers",
    "/docs/semantic-navigation": "/docs/how-it-works/",
    "/docs/cost-receipts": "/docs/cost-saving/",
    "/docs/troubleshooting": "/docs/help/"
  }
});
