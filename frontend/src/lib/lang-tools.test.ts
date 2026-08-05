import { describe, it, expect } from 'vitest';
import { lspToRows, lspFromRows, fmtToRows, fmtFromRows, layerLabel } from './lang-tools';
import type { LspConfigView } from '$lib/types/LspConfigView';
import type { FormatConfigView } from '$lib/types/FormatConfigView';
import type { LspServerView } from '$lib/types/LspServerView';
import type { FormatterView } from '$lib/types/FormatterView';

// A representative registry row (pyright) + a tombstoned one (ruff) + a
// user-defined server, exercising the three row shapes the checklist renders.
const LSP_VIEW: LspConfigView = {
	servers: [
		{
			layer: 'builtin',
			builtin: true,
			installed: true,
			name: 'pyright',
			command: 'pyright-langserver',
			args: ['--stdio'],
			env: {},
			extensions: ['py', 'pyi'],
			enabled: true,
			diag_timeout_ms: 400n,
			init_timeout_ms: 2000n
		},
		{
			layer: 'workspace',
			builtin: true,
			installed: false,
			name: 'ruff',
			command: 'ruff',
			args: ['server'],
			env: {},
			extensions: ['py', 'pyi'],
			enabled: false, // tombstoned by a higher layer — stays visible, greyed
			diag_timeout_ms: 400n,
			init_timeout_ms: 2000n
		},
		{
			layer: 'global',
			builtin: false,
			installed: true,
			name: 'custom-ls',
			command: 'custom-ls',
			args: [],
			env: {},
			extensions: ['xyz'],
			enabled: true,
			diag_timeout_ms: 800n,
			init_timeout_ms: 3000n
		}
	]
};

const FMT_VIEW: FormatConfigView = {
	mode: 'edit',
	formatters: [
		{
			layer: 'builtin',
			builtin: true,
			installed: true,
			name: 'rustfmt',
			command: 'rustfmt',
			args: ['--emit', 'stdout'],
			env: {},
			extensions: ['rs'],
			enabled: true,
			supports_line_range: true,
			format_timeout_ms: 2000n
		},
		{
			layer: 'builtin',
			builtin: true,
			installed: false,
			name: 'black',
			command: 'black',
			args: ['-'],
			env: {},
			extensions: ['py', 'pyi'],
			enabled: true,
			supports_line_range: false,
			format_timeout_ms: 2000n
		}
	]
};

describe('lspToRows', () => {
	it('keeps a tombstoned built-in as a visible greyed row', () => {
		const rows = lspToRows(LSP_VIEW);
		const ruff = rows.find((r) => r.name === 'ruff');
		// The whole point of the checklist (vs the permission editor's empty
		// tier): a disabled built-in must still render so it can be re-enabled.
		expect(ruff).toBeDefined();
		expect(ruff!.enabled).toBe(false);
		expect(ruff!.builtin).toBe(true);
		expect(ruff!.layer).toBe('workspace');
	});

	it('converts the wire u64 (bigint) timeouts to numbers', () => {
		const rows = lspToRows(LSP_VIEW);
		expect(rows[0].diagTimeoutMs).toBe(400);
		expect(typeof rows[0].diagTimeoutMs).toBe('number');
	});

	it('preserves view order: built-ins first, user-defined after', () => {
		expect(lspToRows(LSP_VIEW).map((r) => r.name)).toEqual(['pyright', 'ruff', 'custom-ls']);
	});
});

describe('fmtToRows', () => {
	it('carries supports_line_range into the row', () => {
		const rows = fmtToRows(FMT_VIEW);
		expect(rows.find((r) => r.name === 'rustfmt')!.supportsLineRange).toBe(true);
		expect(rows.find((r) => r.name === 'black')!.supportsLineRange).toBe(false);
	});
});

describe('round-trip fromRows(toRows(view)) preserves user-owned fields', () => {
	it('LSP: every row round-trips (a full-replacement PUT loses nothing)', () => {
		const rows = lspToRows(LSP_VIEW);
		const edit = lspFromRows(rows);
		expect(edit.servers).toEqual([
			{
				name: 'pyright',
				enabled: true,
				command: 'pyright-langserver',
				diag_timeout_ms: 400,
				init_timeout_ms: 2000
			},
			{
				name: 'ruff',
				enabled: false,
				command: 'ruff',
				diag_timeout_ms: 400,
				init_timeout_ms: 2000
			},
			{
				name: 'custom-ls',
				enabled: true,
				command: 'custom-ls',
				diag_timeout_ms: 800,
				init_timeout_ms: 3000
			}
		]);
	});

	it('Format: mode + rows round-trip', () => {
		const rows = fmtToRows(FMT_VIEW);
		const edit = fmtFromRows(rows, FMT_VIEW.mode);
		expect(edit.mode).toBe('edit');
		expect(edit.formatters.map((f) => [f.name, f.enabled])).toEqual([
			['rustfmt', true],
			['black', true]
		]);
	});

	it('an edit then decompile reflects exactly the edit (dirty-detection contract)', () => {
		const rows = lspToRows(LSP_VIEW);
		rows.find((r) => r.name === 'pyright')!.enabled = false;
		const edit = lspFromRows(rows);
		expect(edit.servers.find((s) => s.name === 'pyright')!.enabled).toBe(false);
		// Untouched rows are unchanged (no accidental writes).
		expect(edit.servers.find((s) => s.name === 'custom-ls')).toEqual({
			name: 'custom-ls',
			enabled: true,
			command: 'custom-ls',
			diag_timeout_ms: 800,
			init_timeout_ms: 3000
		});
	});
});

describe('layerLabel', () => {
	it('labels the three row shapes the checklist shows', () => {
		// The badge is the "which layer is this from" cue; the labels are the
		// user-facing reading of `ConfigLayer`.
		expect(layerLabel('builtin', true)).toBe('内置');
		expect(layerLabel('global', false)).toBe('全局');
		expect(layerLabel('workspace', true)).toBe('项目覆盖');
		expect(layerLabel('workspace', false)).toBe('项目新增');
	});
});

// Type-level guard: the edit bodies must carry ONLY user-owned fields — a
// builtin-set field (args/extensions) leaking into the PUT body would mean the
// server no longer re-derives it from the registry.
describe('edit body shape', () => {
	it('LSP edit rows omit builtin-set fields', () => {
		const edit = lspFromRows(lspToRows(LSP_VIEW));
		for (const s of edit.servers) {
			expect(Object.keys(s).sort()).toEqual([
				'command',
				'diag_timeout_ms',
				'enabled',
				'init_timeout_ms',
				'name'
			]);
		}
	});

	it('Format edit rows omit builtin-set fields', () => {
		const edit = fmtFromRows(fmtToRows(FMT_VIEW), FMT_VIEW.mode);
		for (const f of edit.formatters) {
			expect(Object.keys(f).sort()).toEqual(['command', 'enabled', 'format_timeout_ms', 'name']);
		}
	});
});

// Fixture sanity: these views must satisfy the generated wire types (a drift
// between the fixture and the real binding fails compile, not just a test).
const _typecheck: [LspServerView, FormatterView] = [LSP_VIEW.servers[0], FMT_VIEW.formatters[0]];
void _typecheck;
