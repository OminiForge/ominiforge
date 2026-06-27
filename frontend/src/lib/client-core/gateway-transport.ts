// Web transport: REST over fetch + an SSE event stream read through fetch's
// ReadableStream (not the native EventSource, which cannot send the
// `Authorization` header the gateway requires on every route — gateway.md §5).
//
// Reconnect: we track the last committed seq and resubscribe with a
// `Last-Event-ID` header; the server replays committed events after that seq
// then attaches the live stream (gateway.md §4). Live deltas are not replayed.

import type { SessionMeta } from '$lib/types/SessionMeta';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionSummary } from '$lib/types/SessionSummary';
import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
import { endpoints } from './endpoints';
import type { EventHandlers, EventSubscription, SessionClient } from './types';

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

	async createSession(): Promise<string> {
		const body = await this.#json<{ session_id: string }>(endpoints.sessions(), {
			method: 'POST'
		});
		return body.session_id;
	}

	getSession(id: string): Promise<SessionMeta> {
		return this.#json<SessionMeta>(endpoints.session(id));
	}

	async forkSession(id: string, atSeq: number): Promise<string> {
		const body = await this.#json<{ session_id: string }>(endpoints.fork(id), {
			method: 'POST',
			body: JSON.stringify({ at_seq: atSeq })
		});
		return body.session_id;
	}

	sendMessage(id: string, text: string): Promise<void> {
		return this.#send(endpoints.message(id), {
			method: 'POST',
			body: JSON.stringify({ text })
		});
	}

	cancel(id: string): Promise<void> {
		return this.#send(endpoints.cancel(id), { method: 'POST' });
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

	getRuntime(id: string): Promise<RuntimeInfo> {
		return this.#json<RuntimeInfo>(endpoints.runtime(id));
	}

	subscribeEvents(id: string, handlers: EventHandlers, lastSeq?: number): EventSubscription {
		const url = this.#baseUrl + endpoints.events(id);
		const controller = new AbortController();
		let lastSeen = lastSeq;
		let closed = false;

		const run = async () => {
			while (!closed) {
				try {
					const headers = this.#headers({ Accept: 'text/event-stream' });
					if (lastSeen !== undefined) headers.set('Last-Event-ID', String(lastSeen));

					const res = await fetch(url, { headers, signal: controller.signal });
					if (!res.ok) throw await gatewayError(res);
					if (!res.body) throw new Error('event stream has no body');

					for await (const frame of parseSse(res.body, controller.signal)) {
						if (frame.id !== undefined) lastSeen = Number(frame.id);
						if (frame.data) {
							const event = JSON.parse(frame.data) as GatewayEvent;
							handlers.onEvent(event);
						}
					}
				} catch (err) {
					if (closed || controller.signal.aborted) return;
					handlers.onError?.(err);
				}
				// Reconnect after a brief backoff, resuming from lastSeen.
				if (!closed) await delay(1000);
			}
		};
		void run();

		return {
			close() {
				closed = true;
				controller.abort();
			}
		};
	}
}

/** One parsed SSE frame. */
interface SseFrame {
	id?: string;
	data?: string;
}

/**
 * Parse an SSE byte stream into frames. Handles multi-line `data:` and `id:`
 * fields; a blank line dispatches the accumulated frame.
 */
async function* parseSse(
	body: ReadableStream<Uint8Array>,
	signal: AbortSignal
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
				// Other fields (event:, retry:, comments) are ignored.
			}
		}
	} finally {
		reader.releaseLock();
	}
}

/** Build a structured error from a non-2xx gateway response. */
async function gatewayError(res: Response): Promise<Error> {
	let detail = res.statusText;
	try {
		const body = (await res.json()) as { error?: string };
		if (body.error) detail = body.error;
	} catch {
		// non-JSON body; keep the status text
	}
	return new Error(`gateway ${res.status}: ${detail}`);
}

function headerObj(init?: HeadersInit): Record<string, string> {
	if (!init) return {};
	return Object.fromEntries(new Headers(init).entries());
}

function delay(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}
