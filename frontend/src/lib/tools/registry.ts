// Maps a tool name to the component that renders its result body. Built-in
// tools get dedicated views; everything else (MCP tools, future built-ins) falls
// back to GenericResult. Keep this a plain lookup — not a plugin framework.
import type { Component } from 'svelte';
import ReadResult from '$lib/components/tools/ReadResult.svelte';
import EditResult from '$lib/components/tools/EditResult.svelte';
import WriteResult from '$lib/components/tools/WriteResult.svelte';
import ShellResult from '$lib/components/tools/ShellResult.svelte';
import GenericResult from '$lib/components/tools/GenericResult.svelte';
import type { ToolView } from './view';

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
	/** The backend's UI-only rendering of this call's result (`Content::TextView`,
	 *  `doc/tool-view.md`): the precise diff for `edit`/`write`, or the full
	 *  content for a `write` new file. The front-end renders it verbatim — it
	 *  never rebuilds diffs client-side. `undefined` while running (no view yet)
	 *  and for tools that produce none. */
	view?: string;
	/** The approval-gate preview (`Permission::Requested.preview`): the would-be
	 *  diff/content for `edit`/`write`, shown while the call awaits a human
	 *  decision. Same shape as `view`; the executed `view` replaces it. Only set
	 *  while `approvalPending`. */
	preview?: string;
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

/** Component for a structured `ToolView`'s `kind`, or `null` when the kind has
 *  no dedicated renderer (the caller falls back to the tool-name registry).
 *  `markdown`/`plain` are in the union for future built-ins; no current
 *  backend emits them, so they return `null` here. */
export function viewComponent(view: ToolView): Component<ResultProps> | null {
	switch (view.kind) {
		case 'diff':
			return EditResult as Component<ResultProps>;
		case 'code':
		case 'listing':
			return ReadResult as Component<ResultProps>;
		case 'terminal':
			return ShellResult as Component<ResultProps>;
		default:
			return null;
	}
}
