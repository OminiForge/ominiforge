import { describe, it, expect } from 'vitest';
import { toCards, fromCards, type CardModel } from './permission-cards';
import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
import type { ToolInfo } from '$lib/types/ToolInfo';

const CATALOG: ToolInfo[] = [
	{ name: 'read', label: '读文件', fields: [{ key: 'path', label: '路径', is_path: true }] },
	{ name: 'write', label: '写文件', fields: [{ key: 'path', label: '路径', is_path: true }] },
	{ name: 'shell', label: '运行命令', fields: [{ key: 'command', label: '命令', is_path: false }] }
];

describe('toCards', () => {
	it('tolerates an undefined/empty policy without throwing (regression: {} wire shape)', () => {
		// A never-configured workspace arrives as `{}`, and a normalization miss
		// could hand us `undefined`. Both must degrade to empty cards, not crash.
		expect(() => toCards(undefined as unknown as PermissionPolicy, CATALOG)).not.toThrow();
		const { cards } = toCards({} as PermissionPolicy, CATALOG);
		expect(cards.every((c) => c.default === 'allow' && c.exceptions.length === 0)).toBe(true);
	});

	it('gives every catalog tool a card even with no rules', () => {
		const { cards } = toCards({}, CATALOG);
		expect(cards.map((c) => c.tool)).toEqual(['read', 'write', 'shell']);
		expect(cards.every((c) => c.default === 'allow')).toBe(true);
	});

	it('maps a tool-level deny to the default, not an exception', () => {
		const { cards } = toCards({ deny: [{ tool: 'shell' }] }, CATALOG);
		const shell = cards.find((c) => c.tool === 'shell')!;
		expect(shell.default).toBe('deny');
		expect(shell.exceptions).toHaveLength(0);
	});

	it('deny default wins over ask default for the same tool', () => {
		// A tool-level ask AND a tool-level deny on shell: deny outranks.
		const policy: PermissionPolicy = { deny: [{ tool: 'shell' }], ask: [{ tool: 'shell' }] };
		const shell = toCards(policy, CATALOG).cards.find((c) => c.tool === 'shell')!;
		expect(shell.default).toBe('deny');
	});

	it('maps a field/mode/negate rule to an exception', () => {
		const policy: PermissionPolicy = {
			ask: [{ tool: 'write', field: 'path', mode: 'prefix', negate: true, contains: ['src/'] }]
		};
		const write = toCards(policy, CATALOG).cards.find((c) => c.tool === 'write')!;
		expect(write.exceptions).toEqual([
			{ decision: 'ask', field: 'path', mode: 'prefix', negate: true, values: ['src/'] }
		]);
	});

	it('creates a generic card for a tool not in the catalog', () => {
		const { cards } = toCards({ deny: [{ tool: 'mcp_fs_write', contains: ['/etc'] }] }, CATALOG);
		const mcp = cards.find((c) => c.tool === 'mcp_fs_write')!;
		expect(mcp.info).toBeNull();
		expect(mcp.exceptions).toHaveLength(1);
	});

	it('preserves a rule with an unknown match mode in advanced (forward-compat)', () => {
		// A future backend mode the card UI can't render must ride along untouched,
		// not be silently coerced by the mode dropdown.
		const policy = {
			deny: [{ tool: 'shell', field: 'command', mode: 'glob', contains: ['rm*'] }]
		} as unknown as PermissionPolicy;
		const model = toCards(policy, CATALOG);
		expect(model.advanced).toHaveLength(1);
		expect(model.advanced[0].rule.mode).toBe('glob');
		// The shell card exists but has no exception hijacked from that rule.
		const shell = model.cards.find((c) => c.tool === 'shell')!;
		expect(shell.exceptions).toHaveLength(0);
	});

	it('preserves a wildcard rule in advanced, not a card', () => {
		const policy: PermissionPolicy = { deny: [{ tool: '*', contains: ['/etc/'] }] };
		const model = toCards(policy, CATALOG);
		expect(model.advanced).toEqual([{ list: 'deny', rule: { tool: '*', contains: ['/etc/'] } }]);
		// No card hijacked the wildcard.
		expect(model.cards.every((c) => c.tool !== '*')).toBe(true);
	});
});

describe('round-trip fromCards(toCards(p)) preserves semantics', () => {
	const cases: Record<string, PermissionPolicy> = {
		empty: {},
		'tool-level deny': { deny: [{ tool: 'shell' }] },
		'field substring deny': { deny: [{ tool: 'shell', field: 'command', contains: ['rm -rf'] }] },
		'prefix allow-list ask': {
			ask: [{ tool: 'write', field: 'path', mode: 'prefix', negate: true, contains: ['src/', 'tmp/'] }]
		},
		'wildcard advanced': { deny: [{ tool: '*', contains: ['/etc/'] }] },
		mixed: {
			deny: [{ tool: 'shell', field: 'command', contains: ['sudo'] }, { tool: 'read' }],
			ask: [{ tool: 'write' }]
		}
	};

	for (const [name, policy] of Object.entries(cases)) {
		it(name, () => {
			const round = fromCards(toCards(policy, CATALOG));
			expect(normalize(round)).toEqual(normalize(policy));
		});
	}
});

it('drops an exception with no values (a no-op rule)', () => {
	const model: CardModel = {
		cards: [
			{
				tool: 'shell',
				info: null,
				default: 'allow',
				exceptions: [{ decision: 'deny', field: 'command', mode: 'substring', negate: false, values: [] }]
			}
		],
		advanced: []
	};
	expect(fromCards(model)).toEqual({});
});

// Compare policies by their rule sets regardless of list ordering AND object key
// ordering: the card model applies deny-before-ask (may reorder within a list)
// and rebuilds rule objects with a different key order, but semantics (which
// rules exist in which list) must be identical.
function normalize(p: PermissionPolicy) {
	// Canonical key: sort each rule's own keys so key order doesn't matter.
	const key = (r: Record<string, unknown>) =>
		JSON.stringify(Object.fromEntries(Object.entries(r).sort(([a], [b]) => a.localeCompare(b))));
	return {
		deny: (p.deny ?? []).map(key).sort(),
		ask: (p.ask ?? []).map(key).sort()
	};
}
