// Web transport: REST over fetch + an SSE event stream read through fetch's
// ReadableStream (not the native EventSource, which cannot send the
// `Authorization` header the gateway requires on every route — gateway.md §5).
//
// Reconnect: we track the last committed seq and resubscribe with a
// `Last-Event-ID` header; the server replays committed events after that seq
// then attaches the live stream (gateway.md §4). Live deltas are not replayed.
//
// Stall detection: the server emits a keep-alive comment every 15s
// (KeepAlive::default() in server.rs), so ANY 45s stretch without a single
// byte means the connection is silently dead (a half-open TCP the fetch API
// never errors on — Wi-Fi switch, NAT expiry). The watchdog aborts the fetch
// so the normal catch/reconnect path takes over; without it the UI would
// freeze until a manual refresh.

import type { SessionMeta } from '$lib/types/SessionMeta';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionSummary } from '$lib/types/SessionSummary';
import type { SessionView } from '$lib/types/SessionView';
import type { Message } from '$lib/types/Message';
import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
import type { ProfileSummary } from '$lib/types/ProfileSummary';
import type { ModelSummary } from '$lib/types/ModelSummary';
import type { ProvidersFile } from '$lib/types/ProvidersFile';
import type { ProviderConfig } from '$lib/types/ProviderConfig';
import type { Profile } from '$lib/types/Profile';
import type { WorkspaceSummary } from '$lib/types/WorkspaceSummary';
import type { SessionStatus } from '$lib/types/SessionStatus';
import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
import type { LspConfigView } from '$lib/types/LspConfigView';
import type { FormatConfigView } from '$lib/types/FormatConfigView';
import type { LspConfigEdit, FormatConfigEdit } from '$lib/lang-tools';
import type { WorkspaceConfig } from '$lib/types/WorkspaceConfig';
import type { ToolInfo } from '$lib/types/ToolInfo';
import type { ApprovalScope } from '$lib/types/ApprovalScope';
import { endpoints } from './endpoints';
import type {
	CreateSessionOptions,
	EventHandlers,
	EventSubscription,
	ProviderTestResult,
	ProvidersView,
	ReconfigureOptions,
	SessionClient,
	StatusHandlers
} from './types';

export interface GatewayConfig {
	/** Base URL of the gateway, e.g. `http://127.0.0.1:7878`. No trailing slash. */
	baseUrl: string;
	/** Bearer token; omit for an open (unauthenticated) gateway. */
	token?: string;
}

export class GatewayTransport implements SessionClient {
	readonly #baseUrl: string;
	readonly #token?: string;

	constructor(config: GatewayConfig) {
		this.#baseUrl = config.baseUrl.replace(/\/+$/, '');
		this.#token = config.token;
	}

	#headers(extra?: Record<string, string>): Headers {
		const h = new Headers(extra);
		if (this.#token) h.set('Authorization', `Bearer ${this.#token}`);
		return h;
	}

	async #json<T>(path: string, init?: RequestInit): Promise<T> {
		const res = await fetch(this.#baseUrl + path, {
			...init,
			headers: this.#headers(
				init?.body
					? { 'Content-Type': 'application/json', ...headerObj(init.headers) }
					: headerObj(init?.headers)
			)
		});
		if (!res.ok) throw await gatewayError(res);
		return (await res.json()) as T;
	}

	async #send(path: string, init?: RequestInit): Promise<void> {
		const res = await fetch(this.#baseUrl + path, {
			...init,
			headers: this.#headers(init?.body ? { 'Content-Type': 'application/json' } : undefined)
		});
		if (!res.ok) throw await gatewayError(res);
	}

	async listSessions(): Promise<string[]> {
		const body = await this.#json<{ sessions: string[] }>(endpoints.sessions());
		return body.sessions;
	}

	async listArchivedSessions(workspaceId: string): Promise<SessionMeta[]> {
		const body = await this.#json<{ sessions: SessionMeta[] }>(
			endpoints.workspaceArchivedSessions(workspaceId)
		);
		return body.sessions;
	}

	async listWorkspaces(): Promise<WorkspaceSummary[]> {
		const body = await this.#json<{ workspaces: WorkspaceSummary[] }>(endpoints.workspaces());
		return body.workspaces;
	}

	async listWorkspaceSessions(workspaceId: string): Promise<SessionMeta[]> {
		const body = await this.#json<{ sessions: SessionMeta[] }>(
			endpoints.workspaceSessions(workspaceId)
		);
		return body.sessions;
	}

	async createWorkspace(path: string): Promise<string> {
		const body = await this.#json<{ workspace_id: string }>(endpoints.workspaces(), {
			method: 'POST',
			body: JSON.stringify({ path })
		});
		return body.workspace_id;
	}

	async createWorkspaceSession(
		workspaceId: string,
		opts?: { profile?: string; model?: string }
	): Promise<string> {
		// Overrides ride as query params (`?profile=&model=`), mirroring
		// createSession; the workspace is implicit in the path, resolved
		// server-side from `workspaceId` — no path is ever sent.
		const qs = new URLSearchParams();
		if (opts?.profile) qs.set('profile', opts.profile);
		if (opts?.model) qs.set('model', opts.model);
		const query = qs.toString();
		const base = endpoints.workspaceSessions(workspaceId);
		const path = query ? `${base}?${query}` : base;
		const body = await this.#json<{ session_id: string }>(path, { method: 'POST' });
		return body.session_id;
	}

	async createSession(opts?: CreateSessionOptions): Promise<string> {
		// Overrides ride as query params (not a JSON body): the gateway reads
		// `?profile=&model=&workspace=`, and a no-arg call sends no query string
		// (and no body), which the server parses as all-defaults.
		const qs = new URLSearchParams();
		if (opts?.profile) qs.set('profile', opts.profile);
		if (opts?.model) qs.set('model', opts.model);
		if (opts?.workspace) qs.set('workspace', opts.workspace);
		const query = qs.toString();
		const path = query ? `${endpoints.sessions()}?${query}` : endpoints.sessions();
		const body = await this.#json<{ session_id: string }>(path, { method: 'POST' });
		return body.session_id;
	}

	async listProfiles(): Promise<ProfileSummary[]> {
		const body = await this.#json<{ profiles: ProfileSummary[] }>(endpoints.profiles());
		return body.profiles;
	}

	async listTools(): Promise<ToolInfo[]> {
		const body = await this.#json<{ tools: ToolInfo[] }>(endpoints.tools());
		return body.tools;
	}

	async listWorkspaceTools(workspaceId: string): Promise<ToolInfo[]> {
		const body = await this.#json<{ tools: ToolInfo[] }>(endpoints.workspaceTools(workspaceId));
		return body.tools;
	}

	async listModels(): Promise<ModelSummary[]> {
		const body = await this.#json<{ models: ModelSummary[] }>(endpoints.models());
		return body.models;
	}

	getSession(id: string): Promise<SessionMeta> {
		return this.#json<SessionMeta>(endpoints.session(id));
	}

	archiveSession(id: string): Promise<void> {
		return this.#send(endpoints.archive(id), { method: 'POST' });
	}

	deleteSession(id: string): Promise<void> {
		return this.#send(endpoints.session(id), { method: 'DELETE' });
	}

	async forkSession(id: string, atSeq: number): Promise<string> {
		const body = await this.#json<{ session_id: string }>(endpoints.fork(id), {
			method: 'POST',
			body: JSON.stringify({ at_seq: atSeq })
		});
		return body.session_id;
	}

	async reconfigure(id: string, opts: ReconfigureOptions): Promise<string> {
		// Config changes ride as query params (`?profile=&model=`), mirroring
		// createSession; the new session is seeded with this session's history.
		const qs = new URLSearchParams();
		if (opts.profile) qs.set('profile', opts.profile);
		if (opts.model) qs.set('model', opts.model);
		const query = qs.toString();
		const path = query ? `${endpoints.reconfigure(id)}?${query}` : endpoints.reconfigure(id);
		const body = await this.#json<{ session_id: string }>(path, { method: 'POST' });
		return body.session_id;
	}

	sendMessage(
		id: string,
		text: string,
		opts?: { model?: string; thinkEffort?: string }
	): Promise<void> {
		return this.#send(endpoints.message(id), {
			method: 'POST',
			body: JSON.stringify({ text, model: opts?.model, think_effort: opts?.thinkEffort })
		});
	}

	cancel(id: string): Promise<void> {
		return this.#send(endpoints.cancel(id), { method: 'POST' });
	}

	approve(
		id: string,
		callId: string,
		decision: 'approve' | 'reject',
		scope: ApprovalScope
	): Promise<void> {
		return this.#send(endpoints.approve(id), {
			method: 'POST',
			body: JSON.stringify({ call_id: callId, decision, scope })
		});
	}

	compact(id: string, keepLast?: number): Promise<void> {
		return this.#send(endpoints.compact(id), {
			method: 'POST',
			body: JSON.stringify(keepLast === undefined ? {} : { keep_last: keepLast })
		});
	}

	getSummary(id: string): Promise<SessionSummary> {
		return this.#json<SessionSummary>(endpoints.summary(id));
	}

	getView(id: string): Promise<SessionView> {
		return this.#json<SessionView>(endpoints.view(id));
	}

	getSnapshot(id: string): Promise<Message[]> {
		return this.#json<Message[]>(endpoints.snapshot(id));
	}

	getForkPreview(parentId: string, atSeq: number): Promise<Message[]> {
		return this.#json<Message[]>(endpoints.forkPreview(parentId, atSeq));
	}

	getRuntime(id: string): Promise<RuntimeInfo> {
		return this.#json<RuntimeInfo>(endpoints.runtime(id));
	}

	getProviders(): Promise<ProvidersView> {
		return this.#json<ProvidersView>(endpoints.providers());
	}

	saveProviders(providers: ProvidersFile): Promise<void> {
		return this.#send(endpoints.providers(), {
			method: 'PUT',
			body: JSON.stringify(providers)
		});
	}

	testProvider(name: string, edit?: ProviderConfig, key?: string): Promise<ProviderTestResult> {
		// `#json` adds Content-Type from the presence of a body; passing it here
		// too produced `application/json, application/json`, which axum rejects
		// with 415.
		return this.#json<ProviderTestResult>(endpoints.providerTest(name), {
			method: 'POST',
			body: JSON.stringify({ edit: edit ?? null, key: key ?? null })
		});
	}

	getProfile(name: string): Promise<Profile> {
		return this.#json<Profile>(endpoints.profile(name));
	}

	saveProfile(name: string, profile: Profile): Promise<void> {
		return this.#send(endpoints.profile(name), {
			method: 'PUT',
			body: JSON.stringify(profile)
		});
	}

	deleteProfile(name: string): Promise<void> {
		return this.#send(endpoints.profile(name), { method: 'DELETE' });
	}

	getGatewayPermission(): Promise<PermissionPolicy> {
		return this.#json<PermissionPolicy>(endpoints.gatewayPermission());
	}

	saveGatewayPermission(policy: PermissionPolicy): Promise<void> {
		return this.#send(endpoints.gatewayPermission(), {
			method: 'PUT',
			body: JSON.stringify(policy)
		});
	}

	getLspConfig(): Promise<LspConfigView> {
		return this.#json<LspConfigView>(endpoints.lspConfig());
	}

	saveLspConfig(edit: LspConfigEdit): Promise<void> {
		return this.#send(endpoints.lspConfig(), {
			method: 'PUT',
			body: JSON.stringify(edit)
		});
	}

	getFormatConfig(): Promise<FormatConfigView> {
		return this.#json<FormatConfigView>(endpoints.formatConfig());
	}

	saveFormatConfig(edit: FormatConfigEdit): Promise<void> {
		return this.#send(endpoints.formatConfig(), {
			method: 'PUT',
			body: JSON.stringify(edit)
		});
	}

	getWorkspaceLspConfig(workspaceId: string): Promise<LspConfigView> {
		return this.#json<LspConfigView>(endpoints.workspaceLspConfig(workspaceId));
	}

	saveWorkspaceLspConfig(workspaceId: string, edit: LspConfigEdit): Promise<void> {
		return this.#send(endpoints.workspaceLspConfig(workspaceId), {
			method: 'PUT',
			body: JSON.stringify(edit)
		});
	}

	getWorkspaceFormatConfig(workspaceId: string): Promise<FormatConfigView> {
		return this.#json<FormatConfigView>(endpoints.workspaceFormatConfig(workspaceId));
	}

	saveWorkspaceFormatConfig(workspaceId: string, edit: FormatConfigEdit): Promise<void> {
		return this.#send(endpoints.workspaceFormatConfig(workspaceId), {
			method: 'PUT',
			body: JSON.stringify(edit)
		});
	}

	getWorkspaceConfig(workspaceId: string): Promise<WorkspaceConfig> {
		return this.#json<WorkspaceConfig>(endpoints.workspaceConfig(workspaceId));
	}

	saveWorkspaceConfig(workspaceId: string, config: WorkspaceConfig): Promise<void> {
		return this.#send(endpoints.workspaceConfig(workspaceId), {
			method: 'PUT',
			body: JSON.stringify(config)
		});
	}

	setSecret(provider: string, apiKey: string): Promise<void> {
		return this.#send(endpoints.secret(provider), {
			method: 'PUT',
			body: JSON.stringify({ api_key: apiKey })
		});
	}

	deleteSecret(provider: string): Promise<void> {
		return this.#send(endpoints.secret(provider), { method: 'DELETE' });
	}

	subscribeEvents(id: string, handlers: EventHandlers, lastSeq?: number): EventSubscription {
		const url = this.#baseUrl + endpoints.events(id);
		let lastSeen = lastSeq;
		return this.#sseSubscribe(
			(signal, headers) => {
				if (lastSeen !== undefined) headers.set('Last-Event-ID', String(lastSeen));
				return fetch(url, { headers, signal });
			},
			(frame) => {
				if (frame.id !== undefined) lastSeen = Number(frame.id);
				if (frame.data) handlers.onEvent(JSON.parse(frame.data) as GatewayEvent);
			},
			handlers,
			'event stream has no body'
		);
	}

	subscribeStatus(handlers: StatusHandlers): EventSubscription {
		// Same SSE machinery as subscribeEvents, minus Last-Event-ID: this stream
		// isn't resumed by seq — a reconnect just re-snapshots the current status
		// of every session.
		const url = this.#baseUrl + endpoints.statusEvents();
		return this.#sseSubscribe(
			(signal, headers) => fetch(url, { headers, signal }),
			(frame) => {
				if (frame.data) handlers.onStatus(JSON.parse(frame.data) as SessionStatus);
			},
			handlers,
			'status stream has no body'
		);
	}

	/** One SSE-over-fetch subscription loop, shared by the per-session event
	 *  stream and the gateway-wide status stream: bearer headers, reconnect with
	 *  a 1s backoff, and the stall watchdog (any byte — a frame or a keep-alive
	 *  comment — resets it; 45s of silence aborts the hung fetch so the normal
	 *  catch/reconnect path takes over). `connect` issues the request (the event
	 *  stream's `Last-Event-ID` is the only per-caller difference); `onFrame`
	 *  receives each parsed frame. The returned subscription exposes `reconnect`
	 *  for BOTH streams: callers with positive evidence of a dead connection
	 *  (e.g. a send succeeded but nothing arrived) force an immediate re-attach
	 *  instead of waiting out the watchdog. */
	#sseSubscribe(
		connect: (signal: AbortSignal, headers: Headers) => Promise<Response>,
		onFrame: (frame: SseFrame) => void,
		handlers: {
			onConnection?: (s: 'connecting' | 'connected') => void;
			onError?: (e: unknown) => void;
		},
		noBodyError: string
	): EventSubscription {
		const controller = new AbortController();
		let closed = false;
		let attemptController: AbortController | undefined;
		let lastActivity = Date.now();

		const watchdog = setInterval(() => {
			if (Date.now() - lastActivity > STALL_TIMEOUT_MS) {
				attemptController?.abort();
			}
		}, STALL_CHECK_MS);

		const run = async () => {
			while (!closed) {
				attemptController = new AbortController();
				const signal = AbortSignal.any([controller.signal, attemptController.signal]);
				lastActivity = Date.now();
				handlers.onConnection?.('connecting');
				try {
					const headers = this.#headers({ Accept: 'text/event-stream' });
					const res = await connect(signal, headers);
					if (!res.ok) throw await gatewayError(res);
					if (!res.body) throw new Error(noBodyError);

					handlers.onConnection?.('connected');
					for await (const frame of parseSse(res.body, signal, () => {
						lastActivity = Date.now();
					})) {
						onFrame(frame);
					}
				} catch (err) {
					if (closed || controller.signal.aborted) return;
					handlers.onError?.(err);
				}
				// Reconnect after a brief backoff; the server replays (event stream)
				// or re-snapshots (status stream) from there.
				if (!closed) await delay(1000);
			}
		};
		void run();

		return {
			close() {
				closed = true;
				clearInterval(watchdog);
				controller.abort();
			},
			// Force the current attempt to drop so the loop re-attaches NOW rather
			// than whenever the stall watchdog next fires. Between attempts (the 1s
			// backoff) the stale controller's abort is a harmless no-op; the pending
			// retry IS the reconnect.
			reconnect() {
				attemptController?.abort();
			}
		};
	}
}

/** One parsed SSE frame. */
interface SseFrame {
	id?: string;
	data?: string;
}

/** Watchdog cadence: one interval per subscription, woken every 10s. */
const STALL_CHECK_MS = 10_000;
/** No byte (not even a 15s keep-alive comment) for this long = the connection
 *  is silently dead. 45s tolerates one missed keep-alive plus jitter before
 *  declaring a stall, so a loaded network can't cause a spurious reconnect. */
const STALL_TIMEOUT_MS = 45_000;

/**
 * Parse an SSE byte stream into frames. Handles multi-line `data:` and `id:`
 * fields; a blank line dispatches the accumulated frame. `onActivity`, when
 * given, fires once per received chunk — the stall watchdog's heartbeat, fed
 * by keep-alive comments as well as real frames (a cheap timestamp write, no
 * per-chunk timer churn).
 */
async function* parseSse(
	body: ReadableStream<Uint8Array>,
	signal: AbortSignal,
	onActivity?: () => void
): AsyncGenerator<SseFrame> {
	const reader = body.getReader();
	const decoder = new TextDecoder();
	let buffer = '';
	let dataLines: string[] = [];
	let id: string | undefined;

	try {
		while (!signal.aborted) {
			const { done, value } = await reader.read();
			if (done) break;
			onActivity?.();
			buffer += decoder.decode(value, { stream: true });

			let nl: number;
			while ((nl = buffer.indexOf('\n')) !== -1) {
				const line = buffer.slice(0, nl).replace(/\r$/, '');
				buffer = buffer.slice(nl + 1);

				if (line === '') {
					if (dataLines.length > 0 || id !== undefined) {
						yield { id, data: dataLines.length ? dataLines.join('\n') : undefined };
					}
					dataLines = [];
					id = undefined;
				} else if (line.startsWith('data:')) {
					dataLines.push(line.slice(5).replace(/^ /, ''));
				} else if (line.startsWith('id:')) {
					id = line.slice(3).replace(/^ /, '');
				}
				// Other fields (event:, retry:, comments) are ignored — keep-alive
				// comments among them; their arrival still feeds the stall watchdog
				// via the onActivity chunk callback above.
			}
		}
	} finally {
		reader.releaseLock();
	}
}

/** Build a user-friendly error from a non-2xx gateway response. The raw
 *  server message is kept as `cause` for debugging; the display message is
 *  a plain-language Chinese summary, not a JSON blob. */
async function gatewayError(res: Response): Promise<Error> {
	let detail = res.statusText;
	try {
		const body = (await res.json()) as { error?: string };
		if (body.error) detail = body.error;
	} catch {
		// non-JSON body; keep the status text
	}
	const friendly = friendlyGatewayMessage(res.status, detail);
	const err = new Error(friendly);
	(err as Error & { cause?: string }).cause = `gateway ${res.status}: ${detail}`;
	return err;
}
/** Map an HTTP status + server detail to a plain-language message. The
 *  server detail is often a JSON blob or an internal error string — the
 *  user should see a clean summary, not raw internals. */
function friendlyGatewayMessage(status: number, detail: string): string {
	switch (status) {
		case 400:
			return `请求参数有误：${sanitizeDetail(detail)}`;
		case 401:
			return '认证失败，请检查网关令牌（token）是否正确。';
		case 403:
			return '没有权限执行此操作。';
		case 404:
			return `未找到请求的资源：${sanitizeDetail(detail)}`;
		case 409:
			return `操作冲突：${sanitizeDetail(detail)}`;
		case 429:
			return '请求过于频繁，请稍后再试。';
		case 500:
			return `服务器内部错误：${sanitizeDetail(detail)}`;
		case 502:
		case 503:
			return '网关暂时不可用，请稍后重试。';
		default:
			return `请求失败（${status}）：${sanitizeDetail(detail)}`;
	}
}
/** Trim a server error detail to a reasonable length and strip surrounding
 *  JSON quotes/braces so the user sees plain text, not a raw payload. */
function sanitizeDetail(detail: string): string {
	// If the detail looks like a JSON object (starts with '{'), extract just
	// the error message value to avoid showing raw JSON to the user.
	let text = detail;
	if (text.startsWith('{')) {
		try {
			const parsed = JSON.parse(text) as { error?: string; message?: string };
			text = parsed.error ?? parsed.message ?? text;
		} catch {
			// not valid JSON; use as-is
		}
	}
	return text.length > 120 ? text.slice(0, 120) + '…' : text;
}

function headerObj(init?: HeadersInit): Record<string, string> {
	if (!init) return {};
	return Object.fromEntries(new Headers(init).entries());
}

function delay(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}
