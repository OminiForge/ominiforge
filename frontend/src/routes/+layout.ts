// SPA mode: the gateway has no Node server, so render entirely on the client
// (doc/frontend.md §1). Prerender the shell so adapter-static emits index.html.
export const ssr = false;
export const prerender = true;
