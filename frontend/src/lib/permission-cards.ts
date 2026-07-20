// Pure compile/decompile between the on-disk permission model (two flat rule
// lists, `doc/permission.md` §3) and the card model the UI renders (one card per
// tool: a three-way default + a list of exceptions). Kept out of the Svelte
// component so the mapping — the part that must be correct — is unit-tested.
//
// The disk model is the contract; cards are a view. Round-tripping any policy we
// produced must be lossless. Rules we did NOT produce (wildcards, unknown modes,
// hand-written shapes the cards can't express) are preserved verbatim in an
// `advanced` bucket rather than silently dropped — never lose a user's config.

import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
import type { Rule } from '$lib/types/Rule';
import type { ToolInfo } from '$lib/types/ToolInfo';
import type { MatchMode } from '$lib/types/MatchMode';

/** A tool's default verdict when no exception matches. */
export type Decision = 'allow' | 'ask' | 'deny';

/** One exception on a card: "when <field> <mode> <values> → <decision>". A
 *  negated exception is an allow-list ("when NOT any of <values>"). */
export interface Exception {
	decision: 'ask' | 'deny';
	field: string | null;
	mode: MatchMode;
	negate: boolean;
	values: string[];
}

/** The editable state for one tool's card. */
export interface ToolCard {
	tool: string;
	/** Catalog metadata (label + fields), or null for a tool with no catalog
	 *  entry (e.g. an MCP tool) — rendered as a generic card. */
	info: ToolInfo | null;
	default: Decision;
	exceptions: Exception[];
}

/** The whole editor state: one card per known tool, plus rules the cards can't
 *  represent (preserved, editable only as raw JSON / left alone). */
export interface CardModel {
	cards: ToolCard[];
	/** Rules preserved verbatim because they don't fit the card model. */
	advanced: { list: 'deny' | 'ask'; rule: Rule }[];
}

// A rule's patterns live under `contains` on the wire (alias of `patterns`).
function patternsOf(rule: Rule): string[] {
	return rule.contains ?? [];
}

function modeOf(rule: Rule): MatchMode {
	return rule.mode ?? 'substring';
}

/** Whether a rule is a bare tool-level rule (no patterns, not negated): it sets
 *  the tool's DEFAULT verdict rather than being an exception. */
function isToolLevel(rule: Rule): boolean {
	return patternsOf(rule).length === 0 && !rule.negate;
}

/** Whether the card model can represent this rule as a card exception/default.
 *  Wildcard tool and empty-but-negated oddities go to `advanced`. */
// The match modes the card UI can render. A rule using any other mode (a future
// backend addition) must go to `advanced` untouched, not be silently coerced by
// the mode dropdown — the header comment's promise.
const RENDERABLE_MODES: readonly MatchMode[] = ['substring', 'prefix'];

function isCardable(rule: Rule): boolean {
	if (rule.tool === '*') return false;
	// A negated rule with no patterns never matches — a degenerate shape we don't
	// author; keep it in advanced rather than mis-render it as an exception.
	if (rule.negate && patternsOf(rule).length === 0) return false;
	// An unknown match mode can't be represented by the card's mode control.
	if (!RENDERABLE_MODES.includes(modeOf(rule))) return false;
	return true;
}

/**
 * Compile a policy + tool catalog into the card model. Every catalog tool gets a
 * card (default `allow`) even with no rules, so the user sees every tool. Rules
 * for tools NOT in the catalog still get a (generic) card so nothing is hidden.
 */
export function toCards(policy: PermissionPolicy, tools: ToolInfo[]): CardModel {
	// The wire model omits empty sections, so a caller may hand us `undefined` or
	// `{}` for a never-configured tier; treat both as the empty policy.
	policy ??= {};
	const byTool = new Map<string, ToolCard>();
	const advanced: CardModel['advanced'] = [];

	// Seed a card for every catalog tool, in catalog order.
	for (const info of tools) {
		byTool.set(info.name, {
			tool: info.name,
			info,
			default: 'allow',
			exceptions: []
		});
	}

	const ensureCard = (tool: string): ToolCard => {
		let card = byTool.get(tool);
		if (!card) {
			card = { tool, info: null, default: 'allow', exceptions: [] };
			byTool.set(tool, card);
		}
		return card;
	};

	// deny outranks ask, so apply deny first: a tool-level deny sets default
	// 'deny'; a tool-level ask only sets default 'ask' if deny hasn't already.
	const apply = (list: 'deny' | 'ask', rules: Rule[]) => {
		const decision = list === 'deny' ? 'deny' : 'ask';
		for (const rule of rules) {
			if (!isCardable(rule)) {
				advanced.push({ list, rule });
				continue;
			}
			const card = ensureCard(rule.tool);
			if (isToolLevel(rule)) {
				// Tool-level: sets the default. deny wins over a prior ask default.
				if (decision === 'deny' || card.default === 'allow') {
					card.default = decision;
				}
			} else {
				card.exceptions.push({
					decision,
					field: rule.field ?? null,
					mode: modeOf(rule),
					negate: rule.negate ?? false,
					values: patternsOf(rule)
				});
			}
		}
	};
	apply('deny', policy.deny ?? []);
	apply('ask', policy.ask ?? []);

	return { cards: [...byTool.values()], advanced };
}

/**
 * Decompile the card model back to a policy. The inverse of {@link toCards}:
 * tool-level defaults become bare rules, exceptions become field/mode/negate
 * rules, and the preserved `advanced` rules are merged back into their lists.
 */
export function fromCards(model: CardModel): PermissionPolicy {
	const deny: Rule[] = [];
	const ask: Rule[] = [];

	for (const card of model.cards) {
		// Default verdict → a bare tool-level rule (only when not 'allow', which
		// is the absence of any rule).
		if (card.default === 'deny') deny.push({ tool: card.tool });
		else if (card.default === 'ask') ask.push({ tool: card.tool });

		for (const ex of card.exceptions) {
			// Drop an exception with no values UNLESS it's a negate allow-list —
			// but an empty negate list is inert, so drop that too (no-op rule).
			if (ex.values.length === 0) continue;
			const rule: Rule = { tool: card.tool, contains: ex.values };
			if (ex.field) rule.field = ex.field;
			if (ex.mode !== 'substring') rule.mode = ex.mode;
			if (ex.negate) rule.negate = true;
			(ex.decision === 'deny' ? deny : ask).push(rule);
		}
	}

	// Re-merge preserved advanced rules into their original lists.
	for (const { list, rule } of model.advanced) {
		(list === 'deny' ? deny : ask).push(rule);
	}

	const policy: PermissionPolicy = {};
	if (deny.length) policy.deny = deny;
	if (ask.length) policy.ask = ask;
	return policy;
}
