// Pure compile/decompile between the on-disk permission model (two flat rule
// lists, `doc/permission.md` §3) and the incremental rule-row model the UI
// renders: one row per rule the user actually added — no card per catalog
// tool, no full-list editing. Kept out of the Svelte component so the mapping
// — the part that must be correct — is unit-tested.
//
// The disk model is the contract; rows are a view. Round-tripping any policy
// we produced must be lossless. Rules we can NOT render (a match mode this UI
// predates; the degenerate negate-without-patterns shape) are preserved
// verbatim in an `advanced` bucket rather than silently dropped — never lose a
// user's config.
//

import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
import type { Rule } from '$lib/types/Rule';
import type { MatchMode } from '$lib/types/MatchMode';
import type { ToolInfo } from '$lib/types/ToolInfo';

/** Which list a rule sits in — the list IS the verdict (`doc/permission.md` §2).
 *  Evaluation order: `deny` > `allow` > `ask` (allow exempts from ask, never
 *  from deny). */
export type Decision = 'deny' | 'allow' | 'ask';

/** One editable rule. `values` empty (and not negated) = a tool-level bare
 *  rule, i.e. the tool's default verdict at this tier. */
export interface RuleRow {
	list: Decision;
	/** Tool name; `"*"` = any tool. */
	tool: string;
	/** Restrict the match to one input field; null = search all string values. */
	field: string | null;
	mode: MatchMode;
	/** Allow-list form: match when NO pattern hits. */
	negate: boolean;
	values: string[];
}

/** The whole editor state: the rows the user added, plus rules the row UI
 *  can't represent (preserved, shown read-only). */
export interface RowModel {
	rows: RuleRow[];
	advanced: { list: Decision; rule: Rule }[];
}

// The match modes the row UI can render. A rule using any other mode (a future
// backend addition) must go to `advanced` untouched, not be silently coerced
// by the mode dropdown — the header comment's promise.
const RENDERABLE_MODES: readonly MatchMode[] = ['substring', 'prefix'];

function patternsOf(rule: Rule): string[] {
	return rule.contains ?? [];
}

function modeOf(rule: Rule): MatchMode {
	return rule.mode ?? 'substring';
}

/** Whether the row model can represent this rule faithfully. */
function isRowRenderable(rule: Rule): boolean {
	if (!RENDERABLE_MODES.includes(modeOf(rule))) return false;
	// negate with no patterns never matches — a degenerate shape we don't
	// author; keep it in advanced rather than mis-render or silently drop it.
	if (rule.negate && patternsOf(rule).length === 0) return false;
	return true;
}

/** Compile one disk rule to a row; null when the row UI can't represent it
 *  (see {@link isRowRenderable}) — the caller routes those to `advanced`. */
export function ruleToRow(rule: Rule, list: Decision): RuleRow | null {
	if (!isRowRenderable(rule)) return null;
	const values = patternsOf(rule);
	// Canonicalize: with no patterns (and no negate) a rule is tool-level
	// regardless of field/mode — an empty `contains` matches any input, so
	// `{tool, field}` is the bare rule wearing a hat. Render the bare form.
	const bare = values.length === 0;
	return {
		list,
		tool: rule.tool,
		field: bare ? null : (rule.field ?? null),
		mode: bare ? 'substring' : modeOf(rule),
		negate: rule.negate ?? false,
		values
	};
}

/** Compile a policy into rows. An empty/undefined policy yields no rows —
 *  the empty tier renders as "no rules here", not as a full tool list. Rows
 *  come out in evaluation order (deny, allow, ask) so the list reads as
 *  precedence. */
export function toRows(policy: PermissionPolicy | undefined): RowModel {
	policy ??= {};
	const rows: RuleRow[] = [];
	const advanced: RowModel['advanced'] = [];

	const apply = (list: Decision, rules: Rule[]) => {
		for (const rule of rules) {
			const row = ruleToRow(rule, list);
			if (row) rows.push(row);
			else advanced.push({ list, rule });
		}
	};
	apply('deny', policy.deny ?? []);
	apply('allow', policy.allow ?? []);
	apply('ask', policy.ask ?? []);
	return { rows, advanced };
}

/** Decompile rows back to a policy: the inverse of {@link toRows}. Rows keep
 *  their relative order within each list; preserved `advanced` rules are
 *  appended to their original lists. */
export function fromRows(model: RowModel): PermissionPolicy {
	const deny: Rule[] = [];
	const allow: Rule[] = [];
	const ask: Rule[] = [];
	const lists: Record<Decision, Rule[]> = { deny, allow, ask };

	for (const row of model.rows) {
		// Bare row (no values): a tool-level rule. An empty-values negate row
		// never matches — a no-op rule; drop it rather than write a dead rule.
		if (row.values.length === 0) {
			if (row.negate) continue;
			lists[row.list].push({ tool: row.tool });
			continue;
		}
		const rule: Rule = { tool: row.tool, contains: row.values };
		if (row.field) rule.field = row.field;
		if (row.mode !== 'substring') rule.mode = row.mode;
		if (row.negate) rule.negate = true;
		lists[row.list].push(rule);
	}

	for (const { list, rule } of model.advanced) {
		lists[list].push(rule);
	}

	const policy: PermissionPolicy = {};
	if (deny.length) policy.deny = deny;
	if (allow.length) policy.allow = allow;
	if (ask.length) policy.ask = ask;
	return policy;
}

/** A one-line plain-language reading of a row, so the user never decodes
 *  field + mode + negate mentally — the antidote to the "不属于（白名单）"
 *  double negative. The verdict itself is NOT in the text: callers render it
 *  as a colored badge next to the summary, and saying it twice reads stuttered. */
export function summaryOf(row: RuleRow, catalog: ToolInfo[]): string {
	const info = catalog.find((t) => t.name === row.tool);
	const toolLabel = row.tool === '*' ? 'Any tool' : (info?.label ?? row.tool);

	if (row.values.length === 0 && !row.negate) {
		return `${toolLabel} (whole tool)`;
	}
	const fieldLabel = row.field
		? (info?.fields?.find((f) => f.key === row.field)?.label ?? row.field)
		: 'input';
	const vals = row.values.length ? row.values.join(', ') : '(empty)';
	if (row.negate) {
		const rel = row.mode === 'prefix' ? 'does not start with' : 'does not contain';
		return `${toolLabel}: when ${fieldLabel} ${rel} ${vals} (only these are allowed)`;
	}
	const rel = row.mode === 'prefix' ? 'starts with' : 'contains';
	return `${toolLabel}: when ${fieldLabel} ${rel} ${vals}`;
}

