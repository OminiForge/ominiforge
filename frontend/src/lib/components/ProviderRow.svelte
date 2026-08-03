<script lang="ts">
	import { client } from '$lib/client';
	import { pushToast } from '$lib/toast';
	import ConfirmDialog from './ConfirmDialog.svelte';
	import type { ProviderConfig } from '$lib/types/ProviderConfig';
	import type { ProviderType } from '$lib/types/ProviderType';
	import type { ModelConfig } from '$lib/types/ModelConfig';

	/** One provider row in the settings list. Collapsed = a single quiet line
	 *  (status dot + name + model count + chevron); expanded = an inline panel
	 *  with a model-capability table and the API-key field. The key is validated
	 *  inline (Test next to the input, result shown beside it) — Test verifies
	 *  the *entered* key before it is ever saved. Each row is self-contained and
	 *  never feeds the page SaveBar. Layout follows the OpenRouter list pattern;
	 *  visual tokens follow DESIGN.md (kv labels, hairlines, one accent). */
	let {
		provider,
		builtin = false,
		hasKey,
		onsaved,
		ondeleted
	}: {
		provider: ProviderConfig;
		builtin?: boolean;
		hasKey: boolean;
		onsaved: () => void;
		ondeleted?: () => void;
	} = $props();

	const PROVIDER_TYPES: ProviderType[] = [
		'openai-chat',
		'openai-completion',
		'anthropic',
		'custom'
	];

	// Working copy (custom only). Deep-clone via JSON, NOT structuredClone:
	// Svelte 5 wraps props in a reactive proxy structuredClone can't clone.
	// svelte-ignore state_referenced_locally
	let draft = $state<ProviderConfig>(JSON.parse(JSON.stringify(provider)));
	// svelte-ignore state_referenced_locally
	let snapshot = $state(JSON.stringify(provider));

	let open = $state(false);
	let keyInput = $state('');
	let verifying = $state(false);
	let saving = $state(false);
	let confirmDisconnect = $state(false);
	let testResult = $state<{ ok: boolean; text: string } | null>(null);

	const dirty = $derived(!builtin && JSON.stringify(draft) !== snapshot);
	const connected = $derived(hasKey);
	const models = $derived(builtin ? provider.models : draft.models);
	const modelCount = $derived(models.length);

	// Vendor mark: the provider's official logo, referenced by URL at runtime
	// (never copied into the repo — that sidesteps redistributing a trademarked
	// asset, and the logo is always current). On load failure we fall back to a
	// neutral initial tile. Mapped by provider name; unknown providers go
	// straight to the letter.
	const LOGOS: Record<string, string> = {
		'kimi-code': 'https://platform.kimi.com/favicon.ico',
		'kimi-platform': 'https://platform.kimi.com/favicon.ico',
		'mimo-token': 'https://mimo.mi.com/favicon.png',
		'mimo-api': 'https://mimo.mi.com/favicon.png'
	};
	const INITIALS: Record<string, string> = {
		'kimi-code': 'K',
		'kimi-platform': 'K',
		'mimo-token': 'M',
		'mimo-api': 'M'
	};
	const logoUrl = $derived(LOGOS[provider.name]);
	const initial = $derived(INITIALS[provider.name] ?? (provider.name[0] || '?').toUpperCase());
	// When the remote logo errors (offline, 404, hotlink-blocked) show the tile.
	let logoFailed = $state(false);

	// Compact, aligned capability summary for a model row.
	function fmtCtx(n: number): string {
		return n >= 1_000_000
			? `${(n / 1_000_000).toFixed(n % 1_000_000 === 0 ? 0 : 1)}M`
			: `${Math.round(n / 1000)}K`;
	}
	function fmtOut(n: number): string {
		return n >= 1000 ? `${Math.round(n / 1000)}K` : `${n}`;
	}

	function addModel() {
		const m: ModelConfig = {
			id: '',
			context_window: 128000,
			max_output_tokens: 16384,
			default_temperature: 0,
			thinking: 'none',
			modalities: []
		};
		draft.models = [...draft.models, m];
	}
	function removeModel(mi: number) {
		draft.models = draft.models.filter((_, idx) => idx !== mi);
	}

	// Verify the entered key, and persist it only if the provider accepts it.
	// A single action: a wrong key shows why and never overwrites a stored one;
	// a valid key is saved and confirmed. Reachable only with a non-empty input.
	async function verifyAndSave() {
		const key = keyInput.trim();
		if (!key) return;
		verifying = true;
		testResult = null;
		try {
			const res = await client.testProvider(provider.name, builtin ? undefined : draft, key);
			if (!res.ok) {
				testResult = { ok: false, text: res.error ?? 'connection failed' };
				return;
			}
			await client.setSecret(provider.name, key);
			keyInput = '';
			onsaved();
			testResult = { ok: true, text: `key valid · ${res.model}` };
			pushToast(`${provider.name} connected`, 'success');
		} catch (e) {
			testResult = { ok: false, text: e instanceof Error ? e.message : String(e) };
		} finally {
			verifying = false;
		}
	}

	async function disconnect() {
		confirmDisconnect = false;
		try {
			await client.deleteSecret(provider.name);
			testResult = null;
			pushToast(`${provider.name} disconnected`, 'info');
			onsaved();
		} catch (e) {
			pushToast(e instanceof Error ? e.message : String(e), 'error');
		}
	}

	// Custom save: persist only the provider's fields (name/base_url/models).
	// It NEVER touches the API key — that is `saveKey`'s job alone.
	async function save() {
		saving = true;
		try {
			const view = await client.getProviders();
			const builtinNames = new Set(view.builtin_names);
			const customs = view.providers.filter((p) => !builtinNames.has(p.name));
			const idx = customs.findIndex((p) => p.name === provider.name);
			if (idx >= 0) customs[idx] = draft;
			else customs.push(draft);
			await client.saveProviders({ providers: customs });
			snapshot = JSON.stringify(draft);
			pushToast(`${draft.name || 'Provider'} saved`, 'success');
			onsaved();
		} catch (e) {
			pushToast(e instanceof Error ? e.message : String(e), 'error');
		} finally {
			saving = false;
		}
	}

	function discard() {
		draft = JSON.parse(snapshot);
	}
</script>

<div class="row" class:open class:connected>
	<div class="head-row">
		<button class="head" onclick={() => (open = !open)} aria-expanded={open}>
			<span class="dot" class:on={connected} aria-hidden="true"></span>
			{#if logoUrl && !logoFailed}
				<img class="logo" src={logoUrl} alt="" loading="lazy" onerror={() => (logoFailed = true)} />
			{:else}
				<span class="tile" aria-hidden="true">{initial}</span>
			{/if}
			<span class="name">{builtin ? provider.name : draft.name || 'New provider'}</span>
			{#if builtin}<span class="kind">built-in</span>{/if}
			<span class="meta">
				{modelCount} model{modelCount === 1 ? '' : 's'}{#if connected}
					· connected{/if}
			</span>
			<span class="chev" aria-hidden="true">›</span>
		</button>
		{#if builtin && connected}
			<!-- Disconnect is a rare, destructive action: a quiet glyph in the row
			     corner (not a red banner), still behind a confirm dialog. -->
			<button
				class="rowact"
				title="Disconnect"
				aria-label="Disconnect"
				onclick={() => (confirmDisconnect = true)}
			>
				⏻
			</button>
		{/if}
	</div>

	{#if open}
		<div class="body">
			{#if builtin}
				<div class="endpoint">
					<span class="k">endpoint</span><span class="v">{provider.base_url}</span>
					<span class="k">env</span><span class="v"><code>{provider.api_key_env}</code></span>
				</div>
			{/if}

			{#if builtin}
				<!-- Model capabilities: aligned id + ctx/out/temp so the user can see
				     what each model can do, not just its name. -->
				<div class="mtable">
					<div class="mhead">
						<span class="c-id k">model</span>
						<span class="c-num k">context</span>
						<span class="c-num k">max out</span>
						<span class="c-cap k">thinking</span>
						<span class="c-cap k">input</span>
					</div>
					{#each models as m (m.id)}
						<div class="mrow">
							<span class="c-id v id">{m.id}</span>
							<span class="c-num v">{fmtCtx(m.context_window)}</span>
							<span class="c-num v">{fmtOut(m.max_output_tokens)}</span>
							<span class="c-cap">
								{#if m.thinking === 'always'}<span class="cap on" title="Always reasons">think</span
									>{:else if m.thinking === 'optional'}<span
										class="cap opt"
										title="Reasoning optional">think?</span
									>{:else}<span class="cap off">—</span>{/if}
							</span>
							<span class="c-cap">
								{#if (m.modalities ?? []).length > 0}
									<span class="cap on" title="Text + {(m.modalities ?? []).join(', ')}"
										>{(m.modalities ?? []).length + 1}×</span
									>
								{:else}<span class="cap off">text</span>{/if}
							</span>
						</div>
					{/each}
				</div>
			{:else}
				<div class="grid2">
					<label class="field">
						<span class="k">Type</span>
						<select class="in" bind:value={draft.type}>
							{#each PROVIDER_TYPES as t (t)}<option value={t}>{t}</option>{/each}
						</select>
					</label>
					<label class="field">
						<span class="k">Base URL</span>
						<input
							class="in"
							bind:value={draft.base_url}
							placeholder="https://api…/v1"
							spellcheck="false"
						/>
					</label>
					<label class="field span2">
						<span class="k">api_key_env (fallback)</span>
						<input
							class="in"
							bind:value={draft.api_key_env}
							placeholder="OPENAI_API_KEY"
							spellcheck="false"
						/>
					</label>
				</div>

				<div class="models-edit">
					<div class="models-head">
						<span class="k">Models</span>
						<button class="mini" onclick={addModel}>+ model</button>
					</div>
					{#each draft.models as m, mi (mi)}
						<div class="model-row">
							<input class="in grow" placeholder="model id" bind:value={m.id} spellcheck="false" />
							<input
								class="in num"
								type="number"
								title="context window"
								bind:value={m.context_window}
							/>
							<input
								class="in num"
								type="number"
								title="max output tokens"
								bind:value={m.max_output_tokens}
							/>
							<input
								class="in num"
								type="number"
								step="0.1"
								title="default temperature"
								bind:value={m.default_temperature}
							/>
							<button class="mini danger" onclick={() => removeModel(mi)} aria-label="Remove model"
								>×</button
							>
						</div>
					{/each}
					{#if draft.models.length === 0}<p class="hint">No models yet.</p>{/if}
				</div>
			{/if}

			<!-- API key: a single verify-and-save action lives inside the input's
			     right edge (a check glyph that lights up once text is entered), so
			     the layout never shifts. Verifying a wrong key shows why and never
			     overwrites a stored key; a valid key is saved and confirmed. -->
			<div class="keyblock">
				<div class="keyrow">
					<input
						class="in"
						type="password"
						placeholder={connected ? '•••••• saved — enter a new key to replace' : 'Paste API key'}
						bind:value={keyInput}
						spellcheck="false"
						autocomplete="off"
					/>
					<button
						class="verify"
						class:ready={keyInput.trim().length > 0}
						onclick={verifyAndSave}
						disabled={verifying || !keyInput.trim()}
						title="Verify & save key"
						aria-label="Verify and save key"
					>
						{#if verifying}…{:else}✓{/if}
					</button>
				</div>
				{#if testResult}
					<p class="result" class:ok={testResult.ok} role="status">
						<span class="rdot" aria-hidden="true"></span>{testResult.text}
					</p>
				{/if}
			</div>

			{#if !builtin}
				<div class="foot">
					{#if ondeleted}<button class="link" onclick={ondeleted}>Delete</button>{/if}
					<span class="spacer"></span>
					{#if dirty}
						<button class="link" onclick={discard} disabled={saving}>Discard</button>
					{/if}
					<button class="primary" onclick={save} disabled={saving || !dirty}>
						{saving ? 'Saving…' : 'Save'}
					</button>
				</div>
			{/if}
		</div>
	{/if}
</div>

{#if confirmDisconnect}
	<ConfirmDialog
		title="Disconnect {provider.name}?"
		confirmLabel="Disconnect"
		danger
		onconfirm={disconnect}
		oncancel={() => (confirmDisconnect = false)}
	>
		This removes the stored API key. Sessions using this provider's models will fail until you
		reconnect.
	</ConfirmDialog>
{/if}

<style>
	.row {
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		overflow: hidden;
		transition: border-color var(--motion-base);
	}

	.row.connected {
		border-color: color-mix(in srgb, var(--state-done) 25%, var(--border-subtle));
	}

	.row.open {
		border-color: var(--border-default);
	}

	.head-row {
		display: flex;
		align-items: center;
	}

	.head {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex: 1;
		min-width: 0;
		padding: var(--space-3) var(--space-4);
		background: none;
		border: none;
		cursor: pointer;
		text-align: left;
		color: var(--text-primary);
		font: inherit;
	}

	.head:hover {
		background: var(--canvas-overlay);
	}

	/* Quiet row-corner action (Disconnect). Muted until hovered; never a red
	   banner. */
	.rowact {
		flex: none;
		border: none;
		background: none;
		color: var(--text-disabled);
		font-size: 13px;
		cursor: pointer;
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-sm);
		transition: color var(--motion-fast);
	}

	.rowact:hover {
		color: var(--state-error-text);
		background: var(--canvas-overlay);
	}

	.dot {
		flex: none;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--text-disabled);
	}

	.dot.on {
		background: var(--state-done);
		box-shadow: 0 0 0 3px var(--state-done-bg);
	}

	/* Vendor logo (remote) and its fallback tile share the same footprint so
	   the row never shifts when one swaps for the other. */
	.logo {
		flex: none;
		width: 18px;
		height: 18px;
		border-radius: var(--radius-sm);
		object-fit: contain;
		display: block;
	}

	/* Vendor tile: a small neutral square carrying the vendor's initial. Token
	   colours only; quiet, sits behind the name. */
	.tile {
		flex: none;
		display: grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border-radius: var(--radius-sm);
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		color: var(--text-secondary);
		font-family: var(--font-mono);
		font-size: 10px;
		font-weight: 600;
	}

	.name {
		font-size: 13px;
		font-weight: 590;
	}

	.kind {
		font-family: var(--font-mono);
		font-size: 9px;
		color: var(--text-tertiary);
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}

	.meta {
		margin-left: auto;
		font-size: 11px;
		font-family: var(--font-mono);
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
	}

	.chev {
		flex: none;
		color: var(--text-tertiary);
		font-size: 14px;
		transition: transform var(--motion-fast);
	}

	.row.open .chev {
		transform: rotate(90deg);
	}

	.body {
		border-top: 1px solid var(--border-subtle);
		padding: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		background: var(--canvas-base);
	}

	/* kv labels + values (Detail Rail vocabulary) */
	.k {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
		line-height: 1;
	}

	.v {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		font-variant-numeric: tabular-nums;
	}

	.endpoint {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: var(--space-2) var(--space-3);
		align-items: baseline;
	}

	.endpoint .v {
		overflow-wrap: break-word;
	}

	.endpoint code {
		color: var(--text-secondary);
	}

	/* Model capability table */
	.mtable {
		display: flex;
		flex-direction: column;
		border-top: 1px solid var(--border-subtle);
	}

	.mhead,
	.mrow {
		display: grid;
		grid-template-columns: 1fr 60px 60px 72px 56px;
		gap: var(--space-3);
		align-items: baseline;
		padding: 6px 0;
		border-bottom: 1px solid var(--border-subtle);
	}

	.mhead {
		padding-bottom: var(--space-2);
	}

	.mrow:last-child {
		border-bottom: none;
	}

	.c-num {
		text-align: right;
	}

	.c-cap {
		text-align: left;
	}

	.c-id.id {
		color: var(--text-primary);
	}

	/* Capability markers in the model table. `on` = present (reasoning-tinted),
	   `opt` = optional (muted), `off` = absent (disabled). */
	.cap {
		font-family: var(--font-mono);
		font-size: 10px;
		padding: 1px 5px;
		border-radius: var(--radius-sm);
	}

	.cap.on {
		color: var(--reasoning-text);
		background: var(--reasoning-bg);
		border: 1px solid var(--reasoning-border);
	}

	.cap.opt {
		color: var(--text-tertiary);
		background: var(--canvas-overlay);
	}

	.cap.off {
		color: var(--text-disabled);
	}

	.grid2 {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: var(--space-3);
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}

	.field.span2 {
		grid-column: 1 / -1;
	}

	.in {
		width: 100%;
		padding: 6px 8px;
		background: var(--canvas-raised);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		color: var(--text-primary);
		font-family: var(--font-mono);
		font-size: 12px;
		outline: none;
	}

	.in:focus {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	.models-edit {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		border-top: 1px solid var(--border-subtle);
		padding-top: var(--space-3);
	}

	.models-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}

	.model-row {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}

	.grow {
		flex: 1;
		min-width: 0;
	}

	.num {
		max-width: 84px;
	}

	.mini {
		border: none;
		background: none;
		color: var(--text-secondary);
		font-size: 11px;
		cursor: pointer;
		padding: 2px 4px;
		border-radius: var(--radius-sm);
	}

	.mini:hover {
		background: var(--canvas-overlay);
		color: var(--text-primary);
	}

	.mini.danger:hover {
		color: var(--state-error-text);
	}

	.hint {
		font-size: 11px;
		color: var(--text-tertiary);
		margin: 0;
	}

	/* API key block */
	.keyblock {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.keyrow {
		position: relative;
		display: flex;
		align-items: center;
	}

	.keyrow .in {
		flex: 1;
		padding-right: 34px; /* room for the in-field verify glyph */
	}

	/* The single verify-and-save action: a check glyph pinned inside the input's
	   right edge. Muted/disabled until text is entered, so the layout never
	   shifts and an empty field can never overwrite a stored key. */
	.verify {
		position: absolute;
		right: 6px;
		top: 50%;
		transform: translateY(-50%);
		border: none;
		background: none;
		color: var(--text-disabled);
		font-size: 13px;
		width: 24px;
		height: 24px;
		display: grid;
		place-items: center;
		border-radius: var(--radius-sm);
		cursor: not-allowed;
		transition:
			color var(--motion-fast),
			background var(--motion-fast);
	}

	.verify.ready {
		color: var(--accent-ink);
		cursor: pointer;
	}

	.verify.ready:hover:not(:disabled) {
		background: var(--accent-dim);
	}

	.result {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: 0;
		font-size: 11px;
		font-family: var(--font-mono);
		color: var(--state-error-text);
	}

	.result.ok {
		color: var(--state-done-text);
	}

	.rdot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: currentColor;
	}

	/* Footer actions (custom providers): one accent primary; the rest links. */
	.foot {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.spacer {
		flex: 1;
	}

	.primary {
		background: var(--accent);
		border: 1px solid var(--accent);
		color: var(--accent-fg);
		font-size: 13px;
		font-weight: 590;
		padding: 6px 14px;
		border-radius: var(--radius-md);
		cursor: pointer;
		transition: all var(--motion-fast);
	}

	.primary:hover:not(:disabled) {
		background: var(--accent-hover);
		border-color: var(--accent-hover);
	}

	.primary:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.link {
		border: none;
		background: none;
		color: var(--text-secondary);
		font-size: 12px;
		cursor: pointer;
		padding: 4px 2px;
	}

	.link:hover:not(:disabled) {
		color: var(--text-primary);
	}
</style>
