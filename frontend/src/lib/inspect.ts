import type { GatewayEvent } from '$lib/types/GatewayEvent';

/** A committed event as stored in the raw log (deltas are transient and never
 *  replayed, so only `type: 'event'` entries appear here). */
export type RawEvent = GatewayEvent & { type: 'event' };

/** Inspect timeline: the raw log is the full event history, including the
 *  internal events the folded conversation stream never shows (context
 *  injections, permission decisions, hooks, request timing/usage). A bare
 *  category word like "Model" is useless at that density, so each row shows
 *  the variant plus its most identifying field. */
export function inspectDetail(ev: RawEvent): { variant: string; detail: string } {
	const p = ev.payload;
	if (!p) return { variant: 'unknown', detail: '' };
	const [kind, body] = Object.entries(p)[0] as [string, unknown];
	const v = body != null && typeof body === 'object' ? Object.entries(body)[0] : null;
	if (!v) return { variant: kind, detail: '' };
	const [variant, d] = v as [string, Record<string, unknown>];
	const num = (x: unknown) => (typeof x === 'bigint' ? Number(x) : (x as number));
	switch (`${kind}.${variant}`) {
		case 'Turn.Started': {
			const input = (d.input as string) ?? '';
			return {
				variant,
				detail: input ? input.replace(/\s+/g, ' ').slice(0, 60) : '(no user input)'
			};
		}
		case 'Turn.Failed':
			return { variant, detail: (d.reason as string) ?? '' };
		case 'Model.RequestStarted':
			return { variant, detail: `${d.provider}/${d.model}` };
		case 'Model.ContentBlock': {
			const c = (d.content ?? {}) as Record<string, Record<string, string>>;
			const b = Object.entries(c)[0];
			if (!b) return { variant, detail: '' };
			const [bkind, bd] = b;
			if (bkind === 'ToolCall')
				return {
					variant: `Block·${bkind}`,
					detail: `${bd.name}${bd.summary ? ' ' + bd.summary : ''}`.slice(0, 80)
				};
			return {
				variant: `Block·${bkind}`,
				detail: `${(bd.text ?? '').replace(/\s+/g, ' ').length} chars`
			};
		}
		case 'Model.RequestCompleted': {
			const u = d.usage as { input_tokens: number; output_tokens: number };
			return {
				variant,
				detail: `${d.stop_reason} · ${num(d.duration_ms)}ms · in ${u.input_tokens} / out ${u.output_tokens}`
			};
		}
		case 'Model.RequestFailed':
			return { variant, detail: ((d.error as { message?: string })?.message ?? '').slice(0, 80) };
		case 'Tool.Started':
			return { variant, detail: d.tool_name as string };
		case 'Tool.Completed':
			return { variant, detail: `${num(d.duration_ms)}ms · ${num(d.output_bytes)}B` };
		case 'Tool.Failed':
			return { variant, detail: ((d.error as { code?: string })?.code ?? '').slice(0, 80) };
		case 'Session.Created':
			return { variant, detail: `${(d.tools as string[]).length} tools` };
		case 'Session.Forked':
			return { variant, detail: `at #${d.fork_at_seq}` };
		case 'Session.Ended':
			return { variant, detail: d.reason as string };
		case 'Artifact.Created':
			return { variant, detail: `${d.kind} · ${d.media_type} · ${num(d.size)}B` };
		case 'Injection.ContextInjected':
			return { variant, detail: `${d.token_count} tokens` };
		case 'Hook.Executed':
			return {
				variant,
				detail: `${d.hook_name} @ ${d.hook_point} → ${d.outcome} · ${num(d.duration_ms)}ms`
			};
		case 'Permission.Requested':
			return { variant, detail: d.tool_name as string };
		case 'Permission.Decided':
			return { variant, detail: `${d.outcome} by ${d.decided_by}` };
		case 'Error.Raised':
			return {
				variant,
				detail: `${(d as unknown as { code: string }).code}: ${((d as unknown as { message: string }).message ?? '').slice(0, 60)}`
			};
		default:
			return { variant: `${kind}.${variant}`, detail: '' };
	}
}

/** One logical action's timeline rows folded together: a model request
 *  (RequestStarted → ContentBlock* → RequestCompleted) or a tool call
 *  (ToolCall block → Permission ask/decide → Started → Completed). */
export type InspectGroup = {
	key: string;
	/** Display position + jump target = the group's first event. */
	seq: number;
	kind: 'model' | 'tool';
	label: string;
	detail: string;
	events: RawEvent[];
};
export type InspectRow = { type: 'single'; ev: RawEvent } | { type: 'group'; group: InspectGroup };

/** Group the flat event log into rows. Grouping keys come from the events
 *  themselves: request_id links a model request's phases; the tool-call
 *  event id links a tool's phases and its permission gate (Permission
 *  events key on call_id, the ToolCall block carries the same id). Turn
 *  events span the whole conversation, so they stay single rows — grouping
 *  them would swallow everything between Started and Completed. */
export function groupInspectEvents(events: RawEvent[]): InspectRow[] {
	// Pass 1: tool-call event seq → call id (the ToolCall content block is the
	// only place the model-assigned call id appears; Tool/Permission events
	// reference the block by its event seq).
	const callIdByEventSeq = new Map<number, string>();
	for (const ev of events) {
		const p = ev.payload as Record<string, unknown> | undefined;
		const m = p?.Model as Record<string, unknown> | undefined;
		const cb = m?.ContentBlock as { content?: Record<string, unknown> } | undefined;
		const tc = cb?.content?.ToolCall as { id?: string } | undefined;
		if (tc?.id) callIdByEventSeq.set(Number(ev.seq), tc.id);
	}

	const num = (x: unknown) => (typeof x === 'bigint' ? Number(x) : (x as number));
	const groups = new Map<string, InspectGroup & { requestId?: string; callId?: string }>();
	const groupKeyOf = (ev: RawEvent): string | null => {
		const p = ev.payload as Record<string, unknown> | undefined;
		if (!p) return null;
		if (p.Model) {
			const [variant, d] = Object.entries(p.Model as object)[0] as [
				string,
				Record<string, unknown>
			];
			if (variant === 'ContentBlock' && (d.content as Record<string, unknown>)?.ToolCall)
				return null; // tool groups own ToolCall blocks
			return `model:${d.request_id}`;
		}
		if (p.Tool) {
			const d = Object.values(p.Tool as object)[0] as Record<string, unknown>;
			return `tool:${(d.tool_call_event_id as { seq: bigint }).seq}`;
		}
		if (p.Permission) {
			const d = Object.values(p.Permission as object)[0] as Record<string, unknown>;
			return `perm:${d.call_id}`;
		}
		return null;
	};

	// Pass 2: accumulate events into groups, preserving first-seen order.
	for (const ev of events) {
		let key = groupKeyOf(ev);
		if (key?.startsWith('perm:')) {
			// Attach to the tool group whose ToolCall block carries this call id.
			const callId = key.slice(5);
			key = null;
			for (const [eventSeq, id] of callIdByEventSeq) {
				if (id === callId) {
					key = `tool:${eventSeq}`;
					break;
				}
			}
			if (!key) key = `perm:${callId}`; // orphan permission (no block seen): own group
		}
		if (!key) continue;
		let g = groups.get(key);
		if (!g) {
			g = {
				key,
				seq: Number(ev.seq),
				kind: key.startsWith('model:') ? 'model' : 'tool',
				label: '',
				detail: '',
				events: []
			};
			groups.set(key, g);
		}
		g.events.push(ev);
	}

	// Pass 3: summarize each group from its members.
	for (const g of groups.values()) {
		if (g.kind === 'model') {
			let started: Record<string, unknown> | undefined;
			let completed: Record<string, unknown> | undefined;
			let failed: Record<string, unknown> | undefined;
			for (const ev of g.events) {
				const m = (ev.payload as Record<string, Record<string, unknown>>).Model;
				if (m.RequestStarted) started = m.RequestStarted as Record<string, unknown>;
				if (m.RequestCompleted) completed = m.RequestCompleted as Record<string, unknown>;
				if (m.RequestFailed) failed = m.RequestFailed as Record<string, unknown>;
			}
			g.label = 'Request';
			if (started) g.label = `${started.provider}/${started.model}`;
			if (completed) {
				const u = completed.usage as { input_tokens: number; output_tokens: number };
				g.detail = `${completed.stop_reason} · ${num(completed.duration_ms)}ms · in ${u.input_tokens} / out ${u.output_tokens}`;
			} else if (failed) {
				g.detail = `✗ ${num(failed.duration_ms)}ms`;
			} else {
				g.detail = 'running…';
			}
		} else {
			let name = '';
			let completed: Record<string, unknown> | undefined;
			let failedEv: Record<string, unknown> | undefined;
			let permOutcome = '';
			for (const ev of g.events) {
				const p = ev.payload as Record<string, Record<string, unknown>>;
				if (p.Tool?.Started) name = (p.Tool.Started as Record<string, unknown>).tool_name as string;
				if (p.Tool?.Completed) completed = p.Tool.Completed as Record<string, unknown>;
				if (p.Tool?.Failed) failedEv = p.Tool.Failed as Record<string, unknown>;
				if (p.Permission?.Decided)
					permOutcome = (p.Permission.Decided as Record<string, unknown>).outcome as string;
			}
			g.label = `Tool ${name || '(unknown)'}`;
			const parts: string[] = [];
			if (permOutcome) parts.push(permOutcome);
			if (completed)
				parts.push(`${num(completed.duration_ms)}ms · ${num(completed.output_bytes)}B`);
			else if (failedEv) parts.push(`✗ ${num(failedEv.duration_ms)}ms`);
			else parts.push('running…');
			g.detail = parts.join(' · ');
		}
	}

	// Pass 4: emit rows in event order; a group appears at its first event.
	const rows: InspectRow[] = [];
	const emitted = new Set<string>();
	for (const ev of events) {
		let key = groupKeyOf(ev);
		if (key?.startsWith('perm:')) {
			const callId = key.slice(5);
			key = null;
			for (const [eventSeq, id] of callIdByEventSeq) {
				if (id === callId) {
					key = `tool:${eventSeq}`;
					break;
				}
			}
			if (!key) key = `perm:${callId}`;
		}
		if (key && groups.has(key)) {
			if (!emitted.has(key)) {
				emitted.add(key);
				rows.push({ type: 'group', group: groups.get(key)! });
			}
		} else {
			rows.push({ type: 'single', ev });
		}
	}
	return rows;
}
