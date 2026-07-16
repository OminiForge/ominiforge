// Gateway REST/SSE/WS endpoint paths, centralized so a route rename happens in
// one place (doc/phase6-plan.md §1). All are relative to the configured base
// URL held by the transport. The session API is served under `/api/*` so it
// never collides with the SPA's own client-side routes (doc/gateway.md).

const API = '/api';

export const endpoints = {
	workspaces: () => `${API}/workspaces`,
	workspaceSessions: (id: string) => `${API}/workspaces/${encodeURIComponent(id)}/sessions`,
	sessions: () => `${API}/sessions`,
	session: (id: string) => `${API}/sessions/${encodeURIComponent(id)}`,
	fork: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/fork`,
	reconfigure: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/reconfigure`,
	message: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/message`,
	cancel: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/cancel`,
	compact: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/compact`,
	summary: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/summary`,
	snapshot: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/snapshot`,
	forkPreview: (id: string, atSeq: number) =>
		`${API}/sessions/${encodeURIComponent(id)}/fork-preview?at_seq=${atSeq}`,
	runtime: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/runtime`,
	events: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/events`,
	ws: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/ws`,
	/** Gateway-wide session activity status stream (all sessions, all workspaces). */
	statusEvents: () => `${API}/status/events`,
	profiles: () => `${API}/profiles`,
	profile: (name: string) => `${API}/profiles/${encodeURIComponent(name)}`,
	models: () => `${API}/models`,
	providers: () => `${API}/providers`,
	secrets: () => `${API}/secrets`,
	secret: (provider: string) => `${API}/secrets/${encodeURIComponent(provider)}`
} as const;
