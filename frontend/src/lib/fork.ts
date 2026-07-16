// A one-shot handoff for "fork from this turn". Clicking fork on a user message
// doesn't create a session immediately — that would litter the store with an
// empty session if the user never sends (mirroring the draft's lazy-create
// rule). Instead it navigates to the workspace's draft route carrying this
// intent, and the draft's first send performs the actual
// `forkSession(parent, atSeq)` + `sendMessage`.
//
// The draft route and a live session are DIFFERENT route components, so a
// `goto` remounts Conversation — component state can't bridge the two. This
// module holds the intent at module scope across that single navigation. It is
// deliberately NOT persisted (unlike queue.ts): it's consumed once, right after
// the click, within the same SPA session. A refresh drops it, which is correct
// — a reloaded empty draft should just be a plain new session, not a stale fork.

/** The pending "fork from here" hand-off between a session view and the draft
 *  it navigates to. */
export interface PendingFork {
	/** The parent session to branch from. */
	parentId: string;
	/** The parent seq to fork at: the event JUST BEFORE the chosen user message,
	 *  so the branch inherits every turn up to (but not including) it. The chosen
	 *  message's text is pre-filled into the draft input instead, to edit + resend
	 *  as the branch's first turn. */
	atSeq: number;
	/** The chosen user message's text, pre-filled into the draft input. */
	draftText: string;
}

let pending: PendingFork | null = null;

/** Record a pending fork, to be consumed by the draft route's first send.
 *  Overwrites any prior unconsumed intent (a second fork click supersedes). */
export function setPendingFork(fork: PendingFork): void {
	pending = fork;
}

/** Take and clear the pending fork (one-shot). Returns `null` when none is set,
 *  so a plain draft open is unaffected. */
export function takePendingFork(): PendingFork | null {
	const f = pending;
	pending = null;
	return f;
}
