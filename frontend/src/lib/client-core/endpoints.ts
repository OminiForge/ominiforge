// Gateway REST/SSE/WS endpoint paths, centralized so a route rename happens in
// one place (doc/phase6-plan.md §1). All are relative to the configured base
// URL held by the transport. The session API is served under `/api/*` so it
// never collides with the SPA's own client-side routes (doc/gateway.md).

const API = '/api';

export const endpoints = {
	workspaces: () => `${API}/workspaces`,
	workspaceSessions: (id: string) => `${API}/workspaces/${encodeURIComponent(id)}/sessions`,
	/** The workspace's archived sessions (the archived section's read source). */
	workspaceArchivedSessions: (id: string) =>
		`${API}/workspaces/${encodeURIComponent(id)}/sessions/archived`,
	sessions: () => `${API}/sessions`,
	session: (id: string) => `${API}/sessions/${encodeURIComponent(id)}`,
	archive: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/archive`,
	fork: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/fork`,
	reconfigure: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/reconfigure`,
	message: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/message`,
	cancel: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/cancel`,
	approve: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/approve`,
	compact: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/compact`,
	summary: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/summary`,
	view: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/view`,
	snapshot: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/snapshot`,
	forkPreview: (id: string, atSeq: number) =>
		`${API}/sessions/${encodeURIComponent(id)}/fork-preview?at_seq=${atSeq}`,
	runtime: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/runtime`,
	events: (id: string) => `${API}/sessions/${encodeURIComponent(id)}/events`,
	/** Gateway-wide session activity status stream (all sessions, all workspaces). */
	statusEvents: () => `${API}/status/events`,
	profiles: () => `${API}/profiles`,
	profile: (name: string) => `${API}/profiles/${encodeURIComponent(name)}`,
	models: () => `${API}/models`,
	/** Built-in tool catalog for the permission-config UI (labels + fields). */
	tools: () => `${API}/tools`,
	providers: () => `${API}/providers`,
	/** Per-workspace config (network + mounts + permission); top tier of the gate. */
	workspaceConfig: (id: string) => `${API}/workspaces/${encodeURIComponent(id)}/config`,
	/** Per-workspace tool catalog (built-ins + this workspace's MCP tools). */
	workspaceTools: (id: string) => `${API}/workspaces/${encodeURIComponent(id)}/tools`,
	/** Gateway-wide baseline permission policy; bottom tier of the gate. */
	gatewayPermission: () => `${API}/gateway/permission`,
	secrets: () => `${API}/secrets`,
	secret: (provider: string) => `${API}/secrets/${encodeURIComponent(provider)}`
} as const;
