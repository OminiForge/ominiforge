// The workspace panel and its sessions are keyed by ids unknown at build time,
// so this subtree is client-rendered only (the SPA fallback serves it).
// Overrides the root prerender=true (doc/frontend.md §1).
export const prerender = false;
