// The transport-agnostic client the UI depends on (doc/frontend.md §2,
// phase6-plan.md §1). Web injects a GatewayTransport; Desktop will inject a
// TauriTransport (Phase 9) behind this same interface.

import type { SessionMeta } from '$lib/types/SessionMeta';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionView } from '$lib/types/SessionView';
import type { SessionSummary } from '$lib/types/SessionSummary';
import type { Message } from '$lib/types/Message';
import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
import type { ProfileSummary } from '$lib/types/ProfileSummary';
import type { ModelSummary } from '$lib/types/ModelSummary';
import type { ProvidersFile } from '$lib/types/ProvidersFile';
import type { Profile } from '$lib/types/Profile';
import type { WorkspaceSummary } from '$lib/types/WorkspaceSummary';
import type { SessionStatus } from '$lib/types/SessionStatus';
import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
import type { WorkspaceConfig } from '$lib/types/WorkspaceConfig';
import type { ToolInfo } from '$lib/types/ToolInfo';
import type { ApprovalScope } from '$lib/types/ApprovalScope';

/** The settings view of `providers.toml`: the raw provider set plus which
 *  providers have an API key in the secret store. Key values are never sent —
 *  the UI shows only whether one is configured (via `secret_names`). */
export interface ProvidersView {
	providers: ProvidersFile['providers'];
	/** Provider names that have a stored API key. */
	secret_names: string[];
}

/** Handle to a live event subscription; call `close()` to detach. */
export interface EventSubscription {
	close(): void;
	/** Abort the in-flight connection attempt so the subscription re-attaches
	 *  immediately (resuming from the last seen seq) instead of waiting for the
	 *  stall watchdog to notice. Optional: a transport whose connections can't
	 *  hang silently may omit it. */
	reconnect?(): void;
}

/** Per-session overrides for {@link SessionClient.createSession}. Each is
 *  optional; an omitted field falls back to the gateway default. The model is a
 *  qualified `provider/model_id`. Overrides apply to that session only and are
 *  never written back to config. */
export interface CreateSessionOptions {
	profile?: string;
	model?: string;
	workspace?: string;
}

/** Config changes for {@link SessionClient.reconfigure}. Workspace is absent —
 *  it is a session property, not reconfigurable (`doc/profile.md` §5). An
 *  omitted field is unchanged from the parent. */
export interface ReconfigureOptions {
	profile?: string;
	model?: string;
}

/** The subscription's transport-level link state. `connecting` covers the
 *  initial connect AND every reconnect after a drop — the UI shows one
 *  "reconnecting" affordance for both; `connected` clears it. */
export type ConnectionState = 'connecting' | 'connected';

/** Callbacks for a session's event stream. */
export interface EventHandlers {
	/** Each committed event or live delta, as the tagged `GatewayEvent` union. */
	onEvent: (event: GatewayEvent) => void;
	/** Transport-level stream error (connection dropped, parse failure). */
	onError?: (error: unknown) => void;
	/** Link-state transitions: `connecting` when a (re)connect attempt starts,
	 *  `connected` once the stream is attached and bytes flow again. */
	onConnection?: (state: ConnectionState) => void;
}

/** Callbacks for the gateway-wide session status stream. */
export interface StatusHandlers {
	/** Each status delta (or snapshot entry) for a session, across all workspaces. */
	onStatus: (status: SessionStatus) => void;
	/** Transport-level stream error (connection dropped, parse failure). */
	onError?: (error: unknown) => void;
}

export interface SessionClient {
	/** Sessions grouped by workspace, most-recently-active first (no-workspace
	 *  group pinned last). The dashboard's read source — grouping is server-side. */
	listWorkspaces(): Promise<WorkspaceSummary[]>;
	/** The session metadata for one workspace (by path-derived id, or `"none"`),
	 *  newest first. Empty for an unknown id — a workspace only exists while a
	 *  session references it. */
	listWorkspaceSessions(workspaceId: string): Promise<SessionMeta[]>;
	/** Register a workspace by absolute path; resolves to its derived id (the
	 *  route the dashboard should open). Rejects if the path does not exist. */
	createWorkspace(path: string): Promise<string>;
	/** Create a session inside the workspace `workspaceId` resolves to; resolves
	 *  to the new session id. The workspace is fixed by the id (its path is
	 *  resolved server-side, never sent), so only profile/model are overridable. */
	createWorkspaceSession(
		workspaceId: string,
		opts?: { profile?: string; model?: string }
	): Promise<string>;
	/** Session ids, newest first. */
	listSessions(): Promise<string[]>;
	/** Metadata for the workspace's archived sessions, newest first — the
	 *  archived section's read source. Workspace-scoped like
	 *  {@link listWorkspaceSessions}: a panel only ever shows its own
	 *  workspace's retired sessions. Archived sessions are absent from every
	 *  active listing; the only remaining action on them is a permanent
	 *  {@link deleteSession}. */
	listArchivedSessions(workspaceId: string): Promise<SessionMeta[]>;
	/** Create a fresh session; resolves to its id. `opts` chooses a per-session
	 *  profile / model override / workspace (each optional → gateway default). */
	createSession(opts?: CreateSessionOptions): Promise<string>;
	/** Profiles available for a new session (name + description). */
	listProfiles(): Promise<ProfileSummary[]>;
	/** Models available for a per-session override, across configured providers. */
	listModels(): Promise<ModelSummary[]>;
	/** Session metadata. */
	getSession(id: string): Promise<SessionMeta>;
	/** Retire a session (`doc/session-storage.md` §9): stops its actor, releases
	 *  its sandbox, and drops it from every active listing. **One-way** — there is
	 *  no unarchive. Its files stay readable, and it can then be permanently
	 *  {@link deleteSession}d. Rejects with 409 if a turn is running. */
	archiveSession(id: string): Promise<void>;
	/** Permanently remove a session's files. **Irreversible.** Requires the
	 *  session to be archived first (a non-archived session rejects with 409) —
	 *  the deliberate two-step confirmation gate. */
	deleteSession(id: string): Promise<void>;
	/** Branch a new session from `id` at parent `atSeq`; resolves to the new id. */
	forkSession(id: string, atSeq: number): Promise<string>;
	/** Materialize a config change (profile / model) as a new session seeded with
	 *  `id`'s full conversation (reconfiguration); resolves to the new session id.
	 *  The session's config is immutable, so this is a new session, not an edit. */
	reconfigure(id: string, opts: ReconfigureOptions): Promise<string>;
	/** Enqueue a turn. Returns once accepted (202); output arrives over the stream. */
	sendMessage(id: string, text: string): Promise<void>;
	/** Abort the running turn, if any. */
	cancel(id: string): Promise<void>;
	/** Answer a suspended `ask` tool call: `approve` runs it, `reject` blocks it
	 *  (`denied_by_user`). `scope` says how far the decision reaches: `once` (this
	 *  call only), `session` (remember for this session), `profile` (persist to
	 *  the profile), `gateway` (persist to the gateway baseline). Idempotent — a
	 *  resolved/unknown `callId` is ignored. */
	approve(
		id: string,
		callId: string,
		decision: 'approve' | 'reject',
		scope: ApprovalScope
	): Promise<void>;
	/** Summarize and switch to a compaction session; `keepLast` keeps recent turns. */
	compact(id: string, keepLast?: number): Promise<void>;
	/** Derived monitor metrics for one session (folded from its committed event log). */
	getSummary(id: string): Promise<SessionSummary>;
	/** The inherited context a forked/compacted/reconfigured session was seeded
	 *  with (its `context_snapshot.json` as a message array), for rendering as
	 *  dimmed history above the live conversation. Rejects (404) for a `new`
	 *  session, which has no snapshot — callers treat that as "no inherited
	 *  context", not an error. */
	getSnapshot(id: string): Promise<Message[]>;
	/** The folded conversation view (render-ready items + the high-water seq),
	 *  computed server-side. This is how a session OPENS: one request, no replay
	 *  stream, no actor spawn — the client renders these items, then subscribes
	 *  to the live stream with `Last-Event-ID: last_seq` so anything committed
	 *  since the fold replays over SSE rather than being lost. */
	getView(id: string): Promise<SessionView>;
	/** The context a fork at `atSeq` WOULD inherit, computed without creating a
	 *  session. Backs the draft branch view so a fork shows its inherited history
	 *  before the first send (no real session, hence no snapshot, exists yet).
	 *  Rejects (404) when `atSeq` precedes any event — treated as "no context". */
	getForkPreview(parentId: string, atSeq: number): Promise<Message[]>;
	/** Config-layer provider/model the gateway resolves for this session (RUNTIME panel). */
	getRuntime(id: string): Promise<RuntimeInfo>;
	/** The full provider set plus which providers have a stored API key (settings UI). */
	getProviders(): Promise<ProvidersView>;
	/** Overwrite `providers.toml` with the given provider set (full desired state). */
	saveProviders(providers: ProvidersFile): Promise<void>;
	/** The raw (unresolved) profile file `name`, for editing in the settings UI. */
	getProfile(name: string): Promise<Profile>;
	/** Overwrite (or create) profile `name`'s file. */
	saveProfile(name: string, profile: Profile): Promise<void>;
	/** Delete profile `name`'s file. */
	deleteProfile(name: string): Promise<void>;
	/** The built-in tool catalog (labels + targetable fields) that drives the
	 *  permission-config UI's per-tool cards (`doc/permission.md` §3.2). */
	listTools(): Promise<ToolInfo[]>;
	/** This workspace's tool catalog: the built-ins plus its MCP tools
	 *  (best-effort; MCP failures degrade to built-ins). Backs the workspace
	 *  config dialog's per-tool cards. */
	listWorkspaceTools(workspaceId: string): Promise<ToolInfo[]>;
	/** The gateway-wide baseline permission policy (bottom tier of the three-tier
	 *  gate, `doc/permission.md` §3.1) — the settings UI's read source. */
	getGatewayPermission(): Promise<PermissionPolicy>;
	/** Replace the gateway baseline policy. Applies to new sessions immediately
	 *  and persists to `gateway.toml`. */
	saveGatewayPermission(policy: PermissionPolicy): Promise<void>;
	/** The per-workspace config (network + mounts + permission; top tier of the
	 *  gate) for the workspace `id` resolves to. A never-configured workspace
	 *  returns `{}` (the backend omits empty sections), so the optional fields may
	 *  be `undefined` — callers must normalize before binding. */
	getWorkspaceConfig(workspaceId: string): Promise<WorkspaceConfig>;
	/** Overwrite the per-workspace config (full desired state). The file lives in
	 *  the gateway's trusted dir, so widening its own `deny` is safe. */
	saveWorkspaceConfig(workspaceId: string, config: WorkspaceConfig): Promise<void>;
	/** Store (or replace) the API key for `provider` in the secret store. */
	setSecret(provider: string, apiKey: string): Promise<void>;
	/** Delete `provider`'s stored API key. */
	deleteSecret(provider: string): Promise<void>;
	/**
	 * Subscribe to a session's events. The transport replays committed events
	 * after `lastSeq` from the durable log, then attaches the live stream
	 * (gateway.md §4); live deltas are not replayed on reconnect.
	 */
	subscribeEvents(id: string, handlers: EventHandlers, lastSeq?: number): EventSubscription;
	/**
	 * Subscribe to the gateway-wide session activity status stream: one feed
	 * carrying every session's `running | awaiting_input | idle` status across
	 * all workspaces (the session list's cross-session read source). The transport
	 * sends a snapshot of current statuses on connect, then live deltas; apply them
	 * idempotently (last-write-wins by session id). Reconnect re-snapshots.
	 */
	subscribeStatus(handlers: StatusHandlers): EventSubscription;
}
