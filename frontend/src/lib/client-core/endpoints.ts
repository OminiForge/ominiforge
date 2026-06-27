// Gateway REST/SSE/WS endpoint paths, centralized so a route rename happens in
// one place (doc/phase6-plan.md §1). All are relative to the configured base
// URL held by the transport. The session API is served under `/api/*` so it
// never collides with the SPA's own client-side routes (doc/gateway.md).

const API = '/api';

export const endpoints = {
	sessions: () => `${API}/sessions`,
	session: (id: string) => `${API}/sessions/${encodeURIComponent(id)}`,
	fork: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/fork`,
	message: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/message`,
	cancel: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/cancel`,
	compact: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/compact`,
	summary: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/summary`,
	runtime: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/runtime`,
	events: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/events`,
	ws: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/ws`
} as const;
