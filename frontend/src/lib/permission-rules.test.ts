import { describe, it, expect } from 'vitest';
import { toRows, fromRows, summaryOf, resolveEffective, type RowModel } from './permission-rules';
import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
import type { ToolInfo } from '$lib/types/ToolInfo';

const CATALOG: ToolInfo[] = [
	{ name: 'read', label: 'Read file', fields: [{ key: 'path', label: 'path', is_path: true }] },
	{ name: 'write', label: 'Write file', fields: [{ key: 'path', label: 'path', is_path: true }] },
	{
		name: 'shell',
		label: 'Run command',
		fields: [{ key: 'command', label: 'command', is_path: false }]
	}
];

describe('toRows', () => {
	it('tolerates an undefined/empty policy and yields no rows', () => {
		// A never-configured tier arrives as `{}` or undefined — the empty tier
		// renders as "no rules", not as a full tool list (the old card bug).
		expect(toRows(undefined).rows).toEqual([]);
		expect(toRows({}).rows).toEqual([]);
		expect(toRows({}).advanced).toEqual([]);
	});

	it('maps a bare tool-level rule to a row with empty values (a tool default)', () => {
		const { rows } = toRows({ deny: [{ tool: 'shell' }] });
		expect(rows).toEqual([
			{ list: 'deny', tool: 'shell', field: null, mode: 'substring', negate: false, values: [] }
		]);
	});

	it('keeps deny and ask rows for the same tool as separate rows', () => {
		const { rows } = toRows({ deny: [{ tool: 'shell' }], ask: [{ tool: 'shell' }] });
		expect(rows).toHaveLength(2);
		expect(rows.map((r) => r.list)).toEqual(['deny', 'ask']);
	});

	it('maps a field/mode/negate rule to a conditioned row', () => {
		const { rows } = toRows({
			ask: [{ tool: 'write', field: 'path', mode: 'prefix', negate: true, contains: ['src/'] }]
		});
		expect(rows).toEqual([
			{
				list: 'ask',
				tool: 'write',
				field: 'path',
				mode: 'prefix',
				negate: true,
				values: ['src/']
			}
		]);
	});

	it('renders a wildcard rule as a row with tool "*"', () => {
		const { rows, advanced } = toRows({ deny: [{ tool: '*', contains: ['/etc/'] }] });
		expect(advanced).toEqual([]);
		expect(rows[0].tool).toBe('*');
		expect(rows[0].values).toEqual(['/etc/']);
	});

	it('preserves a rule with an unknown match mode in advanced (forward-compat)', () => {
		const policy = {
			deny: [{ tool: 'shell', field: 'command', mode: 'glob', contains: ['rm*'] }]
		} as unknown as PermissionPolicy;
		const model = toRows(policy);
		expect(model.rows).toEqual([]);
		expect(model.advanced).toHaveLength(1);
		expect(model.advanced[0].rule.mode).toBe('glob');
	});

	it('canonicalizes a pattern-less rule with a field to the bare form', () => {
		// `{tool, field}` with empty `contains` matches any input — semantically
		// the bare rule; ingest normalizes it so the UI never shows a phantom
		// condition, and the write-back drops the hat.
		const { rows } = toRows({ deny: [{ tool: 'shell', field: 'command' }] });
		expect(rows[0].field).toBeNull();
		expect(rows[0].values).toEqual([]);
		expect(fromRows(toRows({ deny: [{ tool: 'shell', field: 'command' }] }))).toEqual({
			deny: [{ tool: 'shell' }]
		});
	});

	it('preserves a degenerate negate-without-patterns rule in advanced', () => {
		const policy: PermissionPolicy = { ask: [{ tool: 'write', negate: true }] };
		const model = toRows(policy);
		expect(model.rows).toEqual([]);
		expect(model.advanced).toEqual([{ list: 'ask', rule: { tool: 'write', negate: true } }]);
	});
});

describe('fromRows', () => {
	it('compiles an empty-values non-negate row to a bare rule (tool default)', () => {
		const model: RowModel = {
			rows: [
				{ list: 'ask', tool: 'write', field: null, mode: 'substring', negate: false, values: [] }
			],
			advanced: []
		};
		expect(fromRows(model)).toEqual({ ask: [{ tool: 'write' }] });
	});

	it('drops an empty-values negate row (a no-op rule)', () => {
		const model: RowModel = {
			rows: [
				{
					list: 'deny',
					tool: 'shell',
					field: 'command',
					mode: 'substring',
					negate: true,
					values: []
				}
			],
			advanced: []
		};
		expect(fromRows(model)).toEqual({});
	});

	it('omits default-valued fields from the written rule', () => {
		const model: RowModel = {
			rows: [
				{
					list: 'deny',
					tool: 'shell',
					field: null,
					mode: 'substring',
					negate: false,
					values: ['sudo']
				}
			],
			advanced: []
		};
		expect(fromRows(model)).toEqual({ deny: [{ tool: 'shell', contains: ['sudo'] }] });
	});

	it('re-merges advanced rules into their original lists', () => {
		const model: RowModel = {
			rows: [],
			advanced: [{ list: 'deny', rule: { tool: 'shell', mode: 'glob' as never, contains: ['x'] } }]
		};
		expect(fromRows(model).deny).toHaveLength(1);
	});
});

describe('round-trip fromRows(toRows(p)) preserves semantics', () => {
	const cases: Record<string, PermissionPolicy> = {
		empty: {},
		'tool-level deny': { deny: [{ tool: 'shell' }] },
		'field substring deny': { deny: [{ tool: 'shell', field: 'command', contains: ['rm -rf'] }] },
		'prefix allow-list ask': {
			ask: [
				{ tool: 'write', field: 'path', mode: 'prefix', negate: true, contains: ['src/', 'tmp/'] }
			]
		},
		'wildcard row': { deny: [{ tool: '*', contains: ['/etc/'] }] },
		'wildcard bare': { ask: [{ tool: '*' }] },
		'allow rules': {
			allow: [{ tool: 'shell', field: 'command', contains: ['npm install'] }, { tool: 'read' }],
			ask: [{ tool: 'write' }]
		},
		mixed: {
			deny: [{ tool: 'shell', field: 'command', contains: ['sudo'] }, { tool: 'read' }],
			ask: [{ tool: 'write' }]
		}
	};

	for (const [name, policy] of Object.entries(cases)) {
		it(name, () => {
			const round = fromRows(toRows(policy));
			expect(normalize(round)).toEqual(normalize(policy));
		});
	}
});

describe('editor commit write-back (PermissionRulesEditor.commit)', () => {
	// The component seeds rows via toRows, mutates them, then writes every list
	// of fromRows({rows, advanced}) back onto the SAME bound policy object —
	// deny, allow AND ask. These tests pin the contract that flow relies on:
	// allow rules must survive the write-back, and an allow-only edit must
	// change the serialized policy (what the settings page's dirty snapshot
	// compares against).
	const writeBack = (policy: PermissionPolicy, model: RowModel) => {
		const next = fromRows(model);
		policy.deny = next.deny;
		policy.allow = next.allow;
		policy.ask = next.ask;
	};

	it('an edited allow row is still there after the write-back', () => {
		const policy: PermissionPolicy = { allow: [{ tool: 'read' }] };
		const model = toRows(policy);
		// The user opens the row and adds a condition value.
		model.rows[0].values = ['src/'];
		writeBack(policy, model);
		expect(policy.allow).toEqual([{ tool: 'read', contains: ['src/'] }]);
	});

	it('a pure allow-only edit produces a detectable change (dirty)', () => {
		const policy: PermissionPolicy = { deny: [{ tool: 'shell' }] };
		const snapshot = JSON.stringify(policy);
		const model = toRows(policy);
		// The user adds an allow rule and touches nothing else.
		model.rows.push({
			list: 'allow',
			tool: 'read',
			field: null,
			mode: 'substring',
			negate: false,
			values: []
		});
		writeBack(policy, model);
		expect(policy.allow).toEqual([{ tool: 'read' }]);
		expect(JSON.stringify(policy)).not.toBe(snapshot);
	});

	it('removing the last allow row clears the list (undefined, not stale)', () => {
		const policy: PermissionPolicy = { allow: [{ tool: 'read' }] };
		const model = toRows(policy);
		model.rows = model.rows.filter((r) => r.list !== 'allow');
		writeBack(policy, model);
		expect(policy.allow).toBeUndefined();
		expect(JSON.stringify(policy)).toBe('{}');
	});
});

describe('summaryOf', () => {
	it('reads a bare rule as a whole-tool verdict (no verb — the badge carries it)', () => {
		const s = summaryOf(
			{ list: 'deny', tool: 'shell', field: null, mode: 'substring', negate: false, values: [] },
			CATALOG
		);
		expect(s).toBe('Run command (whole tool)');
	});

	it('reads a wildcard tool as Any tool', () => {
		const s = summaryOf(
			{ list: 'deny', tool: '*', field: null, mode: 'substring', negate: false, values: ['/etc/'] },
			CATALOG
		);
		expect(s).toBe('Any tool: when input contains /etc/');
	});

	it('reads a negated prefix rule as an allow-list', () => {
		const s = summaryOf(
			{
				list: 'ask',
				tool: 'write',
				field: 'path',
				mode: 'prefix',
				negate: true,
				values: ['src/', 'tmp/']
			},
			CATALOG
		);
		expect(s).toBe('Write file: when path does not start with src/, tmp/ (only these are allowed)');
	});
});

describe('resolveEffective', () => {
	it('unions deny across tiers with source labels', () => {
		const eff = resolveEffective(
			{ deny: [{ tool: 'shell', contains: ['curl'] }] },
			{ deny: [{ tool: 'read' }] },
			{ deny: [{ tool: 'shell', contains: ['git push'] }] }
		);
		expect(eff.filter((e) => e.list === 'deny').map((e) => e.tier)).toEqual([
			'gateway',
			'profile',
			'workspace'
		]);
	});

	it('ask comes wholesale from the highest tier that sets any', () => {
		const eff = resolveEffective(
			{ ask: [{ tool: 'write' }] },
			{ ask: [{ tool: 'edit' }] },
			{ ask: [{ tool: 'shell' }] }
		);
		const asks = eff.filter((e) => e.list === 'ask');
		expect(asks).toHaveLength(1);
		expect(asks[0].tier).toBe('workspace');
		expect(asks[0].rule.tool).toBe('shell');
	});

	it('falls through to profile ask when workspace sets none', () => {
		const eff = resolveEffective({ ask: [{ tool: 'write' }] }, { ask: [{ tool: 'edit' }] }, {});
		const asks = eff.filter((e) => e.list === 'ask');
		expect(asks.map((a) => a.rule.tool)).toEqual(['edit']);
		expect(asks[0].tier).toBe('profile');
	});

	it('empty tiers everywhere means everything allowed', () => {
		expect(resolveEffective({}, undefined, {})).toEqual([]);
	});

	it('deny union still reports when ask comes from a higher tier', () => {
		// A workspace ask does NOT wash out a gateway deny — different semantics
		// per list (deny unions, ask replaces). Both must appear.
		const eff = resolveEffective({ deny: [{ tool: 'shell' }] }, {}, { ask: [{ tool: 'write' }] });
		expect(eff.some((e) => e.list === 'deny' && e.tier === 'gateway')).toBe(true);
		expect(eff.some((e) => e.list === 'ask' && e.tier === 'workspace')).toBe(true);
	});

	it('allow unions across tiers with source labels', () => {
		const eff = resolveEffective(
			{ allow: [{ tool: 'shell', contains: ['npm install'] }] },
			{},
			{ allow: [{ tool: 'read' }] }
		);
		const allows = eff.filter((e) => e.list === 'allow');
		expect(allows.map((a) => a.tier)).toEqual(['gateway', 'workspace']);
	});
});

// Compare policies by their rule sets regardless of list ordering AND object key
// ordering: rows preserve order but rebuild rule objects with a different key
// order; semantics (which rules exist in which list) must be identical.
function normalize(p: PermissionPolicy) {
	const key = (r: Record<string, unknown>) =>
		JSON.stringify(Object.fromEntries(Object.entries(r).sort(([a], [b]) => a.localeCompare(b))));
	return {
		deny: (p.deny ?? []).map(key).sort(),
		allow: (p.allow ?? []).map(key).sort(),
		ask: (p.ask ?? []).map(key).sort()
	};
}
