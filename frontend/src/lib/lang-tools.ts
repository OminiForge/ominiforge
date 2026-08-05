// Pure compile/decompile between the layered LSP/format settings views the
// gateway serves (`GET /config/lsp`, `GET /config/format`) and the row models
// the editors mutate (`doc/lsp.md` §7, `doc/format.md` §7). Kept out of the
// Svelte components so the mapping — the part that must be correct — is
// unit-tested with a round-trip (mirrors `permission-rules.ts`'s role for the
// permission editor).
//
// The view is a **registry-driven fixed checklist**, NOT the permission
// editor's incremental user-authored list: every registry entry renders by
// default (a tombstoned one stays visible, greyed, so it can be re-enabled),
// and the `PUT` body carries the complete list back. The wire's `u64` fields
// arrive as `bigint`; the edit bodies convert to `number` (timeouts fit
// comfortably — a 2^53 ms cap is not a real constraint).

import type { LspConfigView } from '$lib/types/LspConfigView';
import type { LspServerView } from '$lib/types/LspServerView';
import type { FormatConfigView } from '$lib/types/FormatConfigView';
import type { FormatterView } from '$lib/types/FormatterView';
import type { FormatMode } from '$lib/types/FormatMode';
import type { ConfigLayer } from '$lib/types/ConfigLayer';

/** One LSP row as the editor mutates it: the full view row (for display —
 *  layer badge, install badge, greyed registry fields) plus the user-owned
 *  fields flattened for two-way binding. */
export interface LspRow {
	name: string;
	enabled: boolean;
	command: string;
	diagTimeoutMs: number;
	initTimeoutMs: number;
	// Display-only, re-derived from the view on save (never from the wire).
	readonly layer: ConfigLayer;
	readonly builtin: boolean;
	readonly installed: boolean;
	readonly args: string[];
	readonly extensions: string[];
}

/** One formatter row as the editor mutates it (same shape as {@link LspRow}). */
export interface FmtRow {
	name: string;
	enabled: boolean;
	command: string;
	formatTimeoutMs: number;
	readonly layer: ConfigLayer;
	readonly builtin: boolean;
	readonly installed: boolean;
	readonly args: string[];
	readonly extensions: string[];
	readonly supportsLineRange: boolean;
}

/** `PUT /config/lsp` body — the server re-derives builtin-set fields
 *  (args/env/extensions) from its own fresh view, so the client sends only the
 *  user-owned ones. */
export interface LspConfigEdit {
	servers: {
		name: string;
		enabled: boolean;
		command: string;
		diag_timeout_ms: number;
		init_timeout_ms: number;
	}[];
}

/** `PUT /config/format` body. `mode: null` leaves the current one untouched. */
export interface FormatConfigEdit {
	mode: FormatMode | null;
	formatters: {
		name: string;
		enabled: boolean;
		command: string;
		format_timeout_ms: number;
	}[];
}

function lspRowOf(s: LspServerView): LspRow {
	return {
		name: s.name,
		enabled: s.enabled,
		command: s.command,
		diagTimeoutMs: Number(s.diag_timeout_ms),
		initTimeoutMs: Number(s.init_timeout_ms),
		layer: s.layer,
		builtin: s.builtin,
		installed: s.installed,
		args: s.args,
		extensions: s.extensions
	};
}

function fmtRowOf(f: FormatterView): FmtRow {
	return {
		name: f.name,
		enabled: f.enabled,
		command: f.command,
		formatTimeoutMs: Number(f.format_timeout_ms),
		layer: f.layer,
		builtin: f.builtin,
		installed: f.installed,
		args: f.args,
		extensions: f.extensions,
		supportsLineRange: f.supports_line_range
	};
}

/** Compile the LSP view into editable rows, in view order (built-ins first,
 *  then user-defined). */
export function lspToRows(view: LspConfigView): LspRow[] {
	return view.servers.map(lspRowOf);
}

/** Compile the format view into editable rows. */
export function fmtToRows(view: FormatConfigView): FmtRow[] {
	return view.formatters.map(fmtRowOf);
}

/** Decompile edited LSP rows into the `PUT` body (user-owned fields only;
 *  every row is sent — full-replacement semantics). */
export function lspFromRows(rows: LspRow[]): LspConfigEdit {
	return {
		servers: rows.map((r) => ({
			name: r.name,
			enabled: r.enabled,
			command: r.command,
			diag_timeout_ms: r.diagTimeoutMs,
			init_timeout_ms: r.initTimeoutMs
		}))
	};
}

/** Decompile edited formatter rows + mode into the `PUT` body. */
export function fmtFromRows(rows: FmtRow[], mode: FormatMode): FormatConfigEdit {
	return {
		mode,
		formatters: rows.map((r) => ({
			name: r.name,
			enabled: r.enabled,
			command: r.command,
			format_timeout_ms: r.formatTimeoutMs
		}))
	};
}

/** Badge text for a row's source layer (`builtin` = untouched registry entry;
 *  otherwise the layer whose config file shadowed or added it). */
export function layerLabel(layer: ConfigLayer, builtin: boolean): string {
	if (layer === 'builtin') return '内置';
	if (layer === 'global') return '全局';
	return builtin ? '项目覆盖' : '项目新增';
}
