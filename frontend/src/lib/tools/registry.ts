// Maps a tool name to the component that renders its result body. Built-in
// tools get dedicated views; everything else (MCP tools, future built-ins) falls
// back to GenericResult. Keep this a plain lookup — not a plugin framework.
import type { Component } from 'svelte';
import ReadResult from '$lib/components/tools/ReadResult.svelte';
import EditResult from '$lib/components/tools/EditResult.svelte';
import WriteResult from '$lib/components/tools/WriteResult.svelte';
import ShellResult from '$lib/components/tools/ShellResult.svelte';
import GenericResult from '$lib/components/tools/GenericResult.svelte';

/** The union of props any result component may receive. Each component reads the
 *  subset it needs (Svelte ignores extra props), so ToolBlock can pass one shape.
 *  **Contract**: components MUST handle `result` being `undefined` (the tool is
 *  still running) — ToolBlock renders Body during execution so the user can see
 *  what is being done (args-derived path, command, etc.). */
export interface ResultProps {
	name: string;
	args: string;
	result?: string;
	/** Debug-only supplementary content (e.g. LSP diagnostics) — never a tool's
	 *  primary content; components that accept it forward it to `RawArgs`. */
	diagnostics?: string;
	status: 'running' | 'done' | 'error';
	error_code?: string;
	/** The conversation-wide file cache (`conversation.ts`'s `fileCache`) — used
	 *  by EditResult/WriteResult to build a contextual diff from args, since the
	 *  backend result no longer carries one (`doc/tool-protocol.md` §11.4). */
	fileCache?: Map<string, string[]>;
	/** For `write` only: the pre-write content snapshot (`Item.prevLines`) — the
	 *  cache itself has already advanced to this call's new content by the time
	 *  this renders, so the "before" side has to travel on the item. */
	prevLines?: string[];
}

const REGISTRY: Record<string, Component<ResultProps>> = {
	read: ReadResult as Component<ResultProps>,
	edit: EditResult as Component<ResultProps>,
	write: WriteResult as Component<ResultProps>,
	shell: ShellResult as Component<ResultProps>
};

/** Result component for a tool name, or GenericResult when unmapped. */
export function resultComponent(name: string): Component<ResultProps> {
	return REGISTRY[name] ?? (GenericResult as Component<ResultProps>);
}
