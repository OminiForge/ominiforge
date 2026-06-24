// Dynamic per-session route: ids are unknown at build time, so this page is
// client-rendered only (the SPA fallback serves it). Overrides the root
// prerender=true (doc/frontend.md §1).
export const prerender = false;
