// The transport-agnostic client the UI depends on (doc/frontend.md §2,
// phase6-plan.md §1). Web injects a GatewayTransport; Desktop will inject a
// TauriTransport (Phase 9) behind this same interface.

import type { SessionMeta } from '$lib/types/SessionMeta';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionSummary } from '$lib/types/SessionSummary';
import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
import type { ProfileSummary } from '$lib/types/ProfileSummary';
import type { ModelSummary } from '$lib/types/ModelSummary';

/** Handle to a live event subscription; call `close()` to detach. */
export interface EventSubscription {
	close(): void;
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

/** Callbacks for a session's event stream. */
export interface EventHandlers {
	/** Each committed event or live delta, as the tagged `GatewayEvent` union. */
	onEvent: (event: GatewayEvent) => void;
	/** Transport-level stream error (connection dropped, parse failure). */
	onError?: (error: unknown) => void;
}

export interface SessionClient {
	/** Session ids, newest first. */
	listSessions(): Promise<string[]>;
	/** Create a fresh session; resolves to its id. `opts` chooses a per-session
	 *  profile / model override / workspace (each optional → gateway default). */
	createSession(opts?: CreateSessionOptions): Promise<string>;
	/** Profiles available for a new session (name + description). */
	listProfiles(): Promise<ProfileSummary[]>;
	/** Models available for a per-session override, across configured providers. */
	listModels(): Promise<ModelSummary[]>;
	/** Session metadata. */
	getSession(id: string): Promise<SessionMeta>;
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
	/** Summarize and switch to a compaction session; `keepLast` keeps recent turns. */
	compact(id: string, keepLast?: number): Promise<void>;
	/** Derived monitor metrics for one session (folded from its committed event log). */
	getSummary(id: string): Promise<SessionSummary>;
	/** Config-layer provider/model the gateway resolves for this session (RUNTIME panel). */
	getRuntime(id: string): Promise<RuntimeInfo>;
	/**
	 * Subscribe to a session's events. The transport replays committed events
	 * after `lastSeq` from the durable log, then attaches the live stream
	 * (gateway.md §4); live deltas are not replayed on reconnect.
	 */
	subscribeEvents(id: string, handlers: EventHandlers, lastSeq?: number): EventSubscription;
}
