// The folded conversation view returned by `GET /sessions/{id}/view`.
// Hand-written (not ts-rs): the response is a server-side composite, and the
// item shape mirrors `Item` in `frontend/src/lib/conversation.ts` — the two
// must stay in lockstep; parity is maintained by hand (see the header
// comment in `src/gateway/view.rs`).
import type { Item } from '$lib/conversation';

export interface SessionView {
	/** Render-ready conversation items (the same shape the client fold
	 *  produces, minus streaming/transient state). */
	items: Item[];
	/** The highest committed seq folded; the client resumes its live stream
	 *  strictly after this (`Last-Event-ID`). */
	last_seq: number | null;
	/** Whether a turn is running at the fold point. Drives the live-turn
	 *  indicator, the Cancel affordance, and send-queueing. */
	turn_running: boolean;
	/** Every distinct model a `RequestStarted` used (the runtime layer), for
	 *  the divergence check against the configured model. */
	runtime_models: string[];
}
