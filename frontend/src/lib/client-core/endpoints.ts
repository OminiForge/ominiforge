// Gateway REST/SSE/WS endpoint paths, centralized so a route rename happens in
// one place (doc/phase6-plan.md §1). All are relative to the configured base
// URL held by the transport.

export const endpoints = {
	sessions: () => `/sessions`,
	session: (id: string) => `/sessions/${encodeURIComponent(id)}`,
	fork: (id: string) => `/sessions/${encodeURIComponent(id)}/fork`,
	message: (id: string) => `/sessions/${encodeURIComponent(id)}/message`,
	cancel: (id: string) => `/sessions/${encodeURIComponent(id)}/cancel`,
	compact: (id: string) => `/sessions/${encodeURIComponent(id)}/compact`,
	events: (id: string) => `/sessions/${encodeURIComponent(id)}/events`,
	ws: (id: string) => `/sessions/${encodeURIComponent(id)}/ws`
} as const;
