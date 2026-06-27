// The transport-agnostic client the UI depends on (doc/frontend.md §2,
// phase6-plan.md §1). Web injects a GatewayTransport; Desktop will inject a
// TauriTransport (Phase 9) behind this same interface.

import type { SessionMeta } from '$lib/types/SessionMeta';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionSummary } from '$lib/types/SessionSummary';
import type { RuntimeInfo } from '$lib/types/RuntimeInfo';

/** Handle to a live event subscription; call `close()` to detach. */
export interface EventSubscription {
	close(): void;
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
	/** Create a fresh session; resolves to its id. */
	createSession(): Promise<string>;
	/** Session metadata. */
	getSession(id: string): Promise<SessionMeta>;
	/** Branch a new session from `id` at parent `atSeq`; resolves to the new id. */
	forkSession(id: string, atSeq: number): Promise<string>;
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
