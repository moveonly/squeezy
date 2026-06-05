import { SITE } from "../config";

const routes = [
  "/",
  "/how-it-works/",
  "/benchmarks/",
  "/support/",
  "/install/",
  "/docs/",
  "/docs/install/",
  "/docs/how-it-works/",
  "/docs/cost-saving/",
  "/docs/cost-saving/understand/",
  "/docs/cost-saving/reuse/",
  "/docs/cost-saving/right-size/",
  "/docs/cost-saving/see-the-bill/",
  "/docs/languages/",
  "/docs/providers/",
  "/docs/config/",
  "/docs/permissions/",
  "/docs/sessions/",
  "/docs/help/"
];

export function GET() {
  const urls = routes
    .map((route) => {
      const loc = new URL(route, SITE.url).toString();
      return `<url><loc>${loc}</loc></url>`;
    })
    .join("");

  return new Response(`<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${urls}</urlset>`, {
    headers: {
      "Content-Type": "application/xml"
    }
  });
}
