<script lang="ts">
	import { onMount } from 'svelte';
	import { client } from '$lib/client';
	import PermissionRulesEditor from '$lib/components/PermissionRulesEditor.svelte';
	import ProviderRow from '$lib/components/ProviderRow.svelte';
	import SaveBar from '$lib/components/SaveBar.svelte';
	import Skeleton from '$lib/components/Skeleton.svelte';
	import Toasts from '$lib/components/Toasts.svelte';
	import { pushToast } from '$lib/toast';
	import PickerSelect from '$lib/components/PickerSelect.svelte';
	import { type SelectOption } from '$lib/components/ModelSelect.svelte';
	import LspConfigEditor from '$lib/components/LspConfigEditor.svelte';
	import FormatConfigEditor from '$lib/components/FormatConfigEditor.svelte';
	import {
		lspToRows,
		lspFromRows,
		fmtToRows,
		fmtFromRows,
		type LspRow,
		type FmtRow
	} from '$lib/lang-tools';
	import type { LspConfigView } from '$lib/types/LspConfigView';
	import type { FormatConfigView } from '$lib/types/FormatConfigView';
	import type { FormatMode } from '$lib/types/FormatMode';
	import type { ProviderConfig } from '$lib/types/ProviderConfig';
	import type { ModelSummary } from '$lib/types/ModelSummary';
	import type { Profile } from '$lib/types/Profile';
	import type { ProfileSummary } from '$lib/types/ProfileSummary';
	import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
	import type { ToolInfo } from '$lib/types/ToolInfo';

	type Tab = 'providers' | 'profiles' | 'global';
	let tab = $state<Tab>('providers');

	// ---- Providers state ----
	// Each provider is a self-contained card that saves itself; this page only
	// holds the list. Custom providers are editable, built-ins render as
	// read-only connect cards. None of this feeds the SaveBar.
	let providers = $state<ProviderConfig[]>([]);
	// Built-in catalog entries (read-only), in catalog order.
	let builtins = $state<ProviderConfig[]>([]);
	// Provider names that already have a stored key (from the secret store).
	let configured = $state<Set<string>>(new Set());

	// ---- Profiles state ----
	let profileList = $state<ProfileSummary[]>([]);
	let selectedName = $state<string>('');
	let profile = $state<Profile | null>(null);
	let profileSnapshot = $state<string | null>(null);
	// Every usable model across providers (drives the profile's default-model
	// dropdown and its reasoning-effort tier list).
	let modelList = $state<ModelSummary[]>([]);

	// ---- 全局设置 tab state ----
	// The gateway-wide defaults (doc/permission.md §3.1 baseline, doc/lsp.md §8,
	// doc/format.md §8): the permission floor + the LSP/format registry
	// checklists. Profile- and workspace-level overrides live on the Profiles
	// tab and in the workspace config dialog respectively — not here. Loaded
	// lazily on first tab open so the providers/profiles path pays nothing.
	let gatewayPolicy = $state<PermissionPolicy | null>(null);
	let gatewaySnapshot = $state<string | null>(null);
	let lspView = $state<LspConfigView | null>(null);
	let lspRows = $state<LspRow[]>([]);
	let lspSnapshot = $state<string | null>(null);
	let fmtView = $state<FormatConfigView | null>(null);
	let fmtRows = $state<FmtRow[]>([]);
	let fmtMode = $state<FormatMode>('file');
	let fmtSnapshot = $state<string | null>(null);
	let globalLoaded = $state(false);

	// Tool catalog for the permission rule editors' tool pickers (gateway
	// baseline here, per-profile on the Profiles tab).
	let toolCatalog = $state<ToolInfo[]>([]);

	// ---- Shared UI ----
	let loading = $state(true);
	let saving = $state(false);
	let error = $state<string | null>(null);

	// Sliding pill behind the active tab: measured from the real button so it
	// tracks text width/font loading instead of hardcoded geometry.
	let tabsEl = $state<HTMLDivElement | null>(null);
	let pill = $state({ x: 0, w: 0 });
	$effect(() => {
		void tab; // re-measure on every tab switch
		const active = tabsEl?.querySelector<HTMLElement>('.tab.active');
		if (active) pill = { x: active.offsetLeft, w: active.offsetWidth };
	});

	// ---- Dirty tracking (SaveBar units: profile + the global units).
	// Providers are intentionally absent — each card saves itself. ----
	// New-profile state has no snapshot (nothing on disk to differ from) — an
	// unsaved draft is dirty by definition, or the SaveBar would never offer
	// to save it.
	const profileDirty = $derived(
		profile !== null && (profileSnapshot === null || JSON.stringify(profile) !== profileSnapshot)
	);
	const gatewayDirty = $derived(
		gatewayPolicy !== null &&
			gatewaySnapshot !== null &&
			JSON.stringify(gatewayPolicy) !== gatewaySnapshot
	);
	// The LSP/format dirty signal is the serialized PUT body, not the rows:
	// rows carry display-only fields (layer/installed) that never change the
	// wire, so diffing them would report phantom edits.
	const lspDirty = $derived(
		lspSnapshot !== null && JSON.stringify(lspFromRows(lspRows)) !== lspSnapshot
	);
	const fmtDirty = $derived(
		fmtSnapshot !== null && JSON.stringify(fmtFromRows(fmtRows, fmtMode)) !== fmtSnapshot
	);
	const dirtyCount = $derived(
		[profileDirty, gatewayDirty, lspDirty, fmtDirty].filter(Boolean).length
	);

	// Intercept page leave with unsaved edits — the SaveBar is always visible,
	// but a route change / tab close is not.
	function guardUnsaved(e: BeforeUnloadEvent) {
		if (dirtyCount > 0) {
			e.preventDefault();
			e.returnValue = '';
		}
	}

	async function loadProviders() {
		const view = await client.getProviders();
		const builtinNames = new Set(view.builtin_names);
		// User-defined entries keep server order; built-ins follow the catalog
		// order given by `builtin_names`, so the card row is stable.
		providers = view.providers.filter((p) => !builtinNames.has(p.name));
		builtins = view.builtin_names
			.map((name) => view.providers.find((p) => p.name === name))
			.filter((p): p is ProviderConfig => p !== undefined);
		configured = new Set(view.secret_names);
	}

	async function loadProfiles() {
		profileList = await client.listProfiles();
	}

	async function loadTools() {
		toolCatalog = await client.listTools();
	}

	// Seed the LSP editor from a fetched view: rows + the dirty baseline. The
	// snapshot is the PUT body of the unedited rows (what the dirty signal
	// diffs against), NOT the rows themselves — rows carry display-only fields
	// (layer/installed) that never change the wire.
	function seedLsp(v: LspConfigView) {
		lspView = v;
		lspRows = lspToRows(v);
		lspSnapshot = JSON.stringify(lspFromRows(lspRows));
	}

	function seedFormat(v: FormatConfigView) {
		fmtView = v;
		fmtRows = fmtToRows(v);
		fmtMode = v.mode;
		fmtSnapshot = JSON.stringify(fmtFromRows(fmtRows, fmtMode));
	}

	// Lazy-load the 全局设置 tab's data the first time it is opened: the gateway
	// permission baseline plus the LSP/format registry checklists.
	$effect(() => {
		if (tab === 'global' && !globalLoaded) {
			globalLoaded = true;
			(async () => {
				const [gw, lsp, fmt] = await Promise.all([
					client.getGatewayPermission(),
					client.getLspConfig(),
					client.getFormatConfig()
				]);
				gatewayPolicy = gw ?? {};
				gatewaySnapshot = JSON.stringify(gatewayPolicy);
				seedLsp(lsp);
				seedFormat(fmt);
			})().catch((e) => {
				error = e instanceof Error ? e.message : String(e);
			});
		}
	});

	onMount(async () => {
		try {
			[modelList] = await Promise.all([
				client.listModels(),
				loadProviders(),
				loadProfiles(),
				loadTools()
			]);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	});

	function flash(msg: string) {
		pushToast(msg, 'success');
	}

	// ---- Provider cards (each saves itself; these manage only the list) ----
	// A new custom provider exists only as a local draft until its card saves.
	function addProvider() {
		providers = [
			...providers,
			{ name: '', type: 'openai-chat', base_url: '', api_key_env: '', models: [] }
		];
	}

	// Delete persists immediately for an already-saved provider (removes it from
	// providers.toml); a never-saved draft is just dropped from the list. A
	// stored key is left in place (harmless; the provider name may be reused).
	async function removeProvider(i: number) {
		const target = providers[i];
		const wasSaved = target.name !== '';
		providers = providers.filter((_, idx) => idx !== i);
		if (wasSaved) {
			try {
				await client.saveProviders({ providers });
				pushToast(`${target.name} removed`, 'info');
			} catch (e) {
				error = e instanceof Error ? e.message : String(e);
				await loadProviders().catch(() => {});
			}
		}
	}

	// ---- Profile editing ----
	async function selectProfile(name: string) {
		error = null;
		selectedName = name;
		profile = null;
		profileSnapshot = null;
		if (!name) return;
		try {
			const p = await client.getProfile(name);
			// The backend omits an empty `[permission]` section; normalize so the
			// rule editor always binds a concrete object.
			p.permission ??= {};
			profile = p;
			profileSnapshot = JSON.stringify(p);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	function newProfile() {
		selectedName = '';
		profileSnapshot = null;
		profile = {
			profile: { name: '', description: null, extends: null },
			prompt: { system: null, system_file: null },
			model: { default: null, fallback: null, think_effort: null },
			tools: { builtin: null, mcp_servers: [], disabled: [] },
			context: { compaction_threshold: null, compaction_model: null, injection_max_tokens: null },
			skills: { enabled: [] },
			memory: { scopes: [], auto_write: null },
			budget: { session_max_usd: null, daily_max_usd: null, warn_at_percent: null },
			hooks: { before_tool: [], after_tool: [] },
			network: { policy: null, allow: [] },
			permission: {}
		};
	}

	async function saveProfile(): Promise<boolean> {
		if (!profile) return true;
		const name = profile.profile.name.trim();
		if (!name) {
			error = 'Profile name is required';
			return false;
		}
		try {
			await client.saveProfile(name, profile);
			await loadProfiles();
			selectedName = name;
			profileSnapshot = JSON.stringify(profile);
			return true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return false;
		}
	}

	async function deleteProfile(name: string) {
		error = null;
		try {
			await client.deleteProfile(name);
			await loadProfiles();
			if (selectedName === name) {
				selectedName = '';
				profile = null;
				profileSnapshot = null;
			}
			flash(`Profile ${name} deleted`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// ---- Permissions tab: gateway / workspace tiers ----
	async function saveGateway(): Promise<boolean> {
		if (!gatewayPolicy) return true;
		try {
			await client.saveGatewayPermission(gatewayPolicy);
			gatewaySnapshot = JSON.stringify(gatewayPolicy);
			return true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return false;
		}
	}

	// ---- 全局设置 save ----
	async function saveLsp(): Promise<boolean> {
		try {
			await client.saveLspConfig(lspFromRows(lspRows));
			// Re-seed from the fresh view: install probes / source layers may have
			// shifted, and this re-baselines the dirty snapshot.
			seedLsp(await client.getLspConfig());
			return true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return false;
		}
	}

	async function saveFormat(): Promise<boolean> {
		try {
			await client.saveFormatConfig(fmtFromRows(fmtRows, fmtMode));
			seedFormat(await client.getFormatConfig());
			return true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return false;
		}
	}

	// ---- SaveBar: save / discard across all dirty units ----
	async function saveAll() {
		error = null;
		saving = true;
		try {
			// Stop at the first failure; already-saved units stay saved.
			if (profileDirty && !(await saveProfile())) return;
			if (gatewayDirty && !(await saveGateway())) return;
			if (lspDirty && !(await saveLsp())) return;
			if (fmtDirty && !(await saveFormat())) return;
			flash('Saved');
		} finally {
			saving = false;
		}
	}

	async function discardAll() {
		error = null;
		// An existing profile reloads from disk; a never-saved draft has nothing
		// to reload — discarding abandons it.
		if (profileDirty && selectedName) await selectProfile(selectedName);
		else if (profileDirty) {
			profile = null;
			profileSnapshot = null;
		}
		if (gatewayDirty) {
			const gw = await client.getGatewayPermission().catch(() => null);
			if (gw !== null) {
				gatewayPolicy = gw ?? {};
				gatewaySnapshot = JSON.stringify(gatewayPolicy);
			}
		}
		// Re-seeding from a fresh view discards the local edits and re-baselines
		// the dirty snapshot in one step.
		if (lspDirty) seedLsp(await client.getLspConfig());
		if (fmtDirty) seedFormat(await client.getFormatConfig());
	}

	// ---- Profile model picker options ----
	// The default model is chosen from the usable models (credentials-resolved
	// server-side), not typed by hand; temperature / max-output stay model
	// metadata, so a profile only references a model plus its effort tier.
	const modelOptions = $derived<SelectOption[]>([
		{ value: '', label: 'None (gateway default)' },
		...modelList.map((m) => ({
			value: `${m.provider}/${m.model_id}`,
			label: m.model_id,
			detail: m.provider
		}))
	]);
	// The model the profile editor currently references (drives which effort
	// tiers the effort picker offers).
	const selectedModelEntry = $derived(
		modelList.find((m) => `${m.provider}/${m.model_id}` === (profile?.model.default ?? ''))
	);
	const effortOptions = $derived<SelectOption[]>([
		{ value: '', label: 'Model default' },
		...(selectedModelEntry?.think_efforts ?? []).map((t) => ({ value: t, label: t }))
	]);

	// Trigger labels for the pickers: the current value, or the empty-value
	// option's label when nothing is selected (mirrors the option list).
	const modelPickerLabel = $derived(
		modelOptions.find((o) => o.value === (profile?.model.default ?? ''))?.label ??
			'None (gateway default)'
	);
	const effortPickerLabel = $derived(
		effortOptions.find((o) => o.value === (profile?.model.think_effort ?? ''))?.label ??
			'Model default'
	);

	// Bridge a nullable-number model field to a text input (empty = null).
	function numOrNull(v: string): number | null {
		const t = v.trim();
		return t === '' ? null : Number(t);
	}
</script>

<svelte:window onbeforeunload={guardUnsaved} />

<div class="page">
	<div class="page-inner">
		<header>
			<h1>Settings</h1>
			<div class="tabs" bind:this={tabsEl}>
				<span
					class="tab-pill"
					style:transform="translateX({pill.x}px)"
					style:width="{pill.w}px"
					aria-hidden="true"
				></span>
				<button class="tab" class:active={tab === 'providers'} onclick={() => (tab = 'providers')}>
					Providers
				</button>
				<button class="tab" class:active={tab === 'profiles'} onclick={() => (tab = 'profiles')}>
					Profiles
				</button>
				<button class="tab" class:active={tab === 'global'} onclick={() => (tab = 'global')}>
					全局设置
				</button>
			</div>
		</header>

		{#if error}<p class="error">{error}</p>{/if}

		{#if loading}
			<p class="muted">Loading…</p>
		{:else if tab === 'providers'}
			<!-- PROVIDERS-SECTION: each card is self-contained and saves itself. -->
			<section class="stack">
				{#if builtins.length > 0}
					<span class="key sect">Built-in services</span>
					{#each builtins as p (p.name)}
						<ProviderRow
							provider={p}
							builtin
							hasKey={configured.has(p.name)}
							onsaved={() => loadProviders().catch(() => {})}
						/>
					{/each}

					<span class="key sect">Custom providers</span>
				{/if}
				{#each providers as p, pi (pi)}
					<ProviderRow
						provider={p}
						hasKey={configured.has(p.name)}
						onsaved={() => loadProviders().catch(() => {})}
						ondeleted={() => removeProvider(pi)}
					/>
				{/each}
				{#if providers.length === 0 && builtins.length > 0}
					<p class="hint">No custom providers yet.</p>
				{/if}

				<div class="actions">
					<button class="btn-ghost" onclick={addProvider}>+ provider</button>
				</div>
			</section>
		{:else if tab === 'profiles'}
			<!-- PROFILES-SECTION -->
			<section class="two-col">
				<aside class="list">
					<button class="btn-ghost full" onclick={newProfile}>+ New profile</button>
					{#each profileList as p (p.name)}
						<div class="list-row" class:active={selectedName === p.name}>
							<button class="list-btn" onclick={() => selectProfile(p.name)}>
								<span class="list-name">{p.name}</span>
								{#if p.description}<span class="list-desc">{p.description}</span>{/if}
							</button>
							<button class="btn-ghost danger sm" onclick={() => deleteProfile(p.name)}>×</button>
						</div>
					{/each}
				</aside>

				<div class="editor">
					{#if profile}
						{@const pf = profile}
						<div class="grid2">
							<label class="field">
								<span class="key">Name</span>
								<input class="in" bind:value={pf.profile.name} spellcheck="false" />
							</label>
							<label class="field">
								<span class="key">Extends</span>
								<input
									class="in"
									value={pf.profile.extends ?? ''}
									oninput={(e) => (pf.profile.extends = e.currentTarget.value || null)}
									placeholder="Parent profile"
									spellcheck="false"
								/>
							</label>
							<label class="field span2">
								<span class="key">Description</span>
								<input
									class="in"
									value={pf.profile.description ?? ''}
									oninput={(e) => (pf.profile.description = e.currentTarget.value || null)}
								/>
							</label>
						</div>

						<label class="field">
							<span class="key">System Prompt</span>
							<textarea
								class="in ta"
								rows="4"
								value={pf.prompt.system ?? ''}
								oninput={(e) => (pf.prompt.system = e.currentTarget.value || null)}
							></textarea>
						</label>

						<div class="grid2">
							<label class="field">
								<span class="key">Default model</span>
								<PickerSelect
									options={modelOptions}
									value={pf.model.default ?? ''}
									onselect={(v) => (pf.model.default = v || null)}
									key="model"
									label={modelPickerLabel}
								/>
							</label>
							<label class="field">
								<span class="key">Thinking effort</span>
								<PickerSelect
									options={effortOptions}
									value={pf.model.think_effort ?? ''}
									onselect={(v) => (pf.model.think_effort = v || null)}
									disabled={!selectedModelEntry ||
										(selectedModelEntry.think_efforts ?? []).length === 0}
									key="effort"
									label={effortPickerLabel}
								/>
							</label>
							<label class="field">
								<span class="key">Compaction threshold</span>
								<input
									class="in"
									value={pf.context.compaction_threshold ?? ''}
									oninput={(e) =>
										(pf.context.compaction_threshold = numOrNull(e.currentTarget.value))}
									placeholder="0.8"
								/>
							</label>
						</div>

						<!-- Profile-tier permission gating (the middle tier, between the
						     gateway floor and any workspace override). Edited here, with
						     the profile it belongs to, not on a separate tab. -->
						<div class="field">
							<span class="key">Permission gating</span>
							{#key selectedName}
								<PermissionRulesEditor bind:policy={pf.permission} tools={toolCatalog} />
							{/key}
						</div>
					{:else}
						<p class="muted">Select a profile on the left to edit, or create a new one.</p>
					{/if}
				</div>
			</section>
		{:else}
			<!-- GLOBAL-SECTION: the gateway-wide defaults in one flat container —
			     permission floor + LSP/format registry checklists. Profile /
			     workspace overrides live on the Profiles tab / workspace dialog. -->
			<section class="stack">
				<!-- Permission floor -->
				<div class="gsect">
					<div class="gsect-head">
						<h2>权限基线</h2>
						<p class="gsect-desc">
							三层裁决的最底层。这里的 deny 是网关的安全底线——任何 profile 或 workspace 都不能放宽它。
						</p>
					</div>
					{#if gatewayPolicy}
						<PermissionRulesEditor
							bind:policy={gatewayPolicy}
							tools={toolCatalog}
							showDefaults
							emptyHint="无规则 —— 未命中即允许"
						/>
					{:else}
						<div class="skel-rows" aria-hidden="true">
							<Skeleton width="100%" height="38px" radius="var(--radius-md)" />
							<Skeleton width="100%" height="28px" />
							<Skeleton width="64%" height="28px" />
						</div>
					{/if}
				</div>

				<!-- LSP checklist -->
				<div class="gsect">
					<div class="gsect-head">
						<h2>语言服务器</h2>
						<p class="gsect-desc">
							内置注册表的完整清单（未安装/未启用的标灰但不隐藏）——勾选启用、禁用写入
							enabled = false 墓碑。command 仅对已安装的二进制可改。
						</p>
					</div>
					{#if lspView}
						<!-- Global view: no install badge / command lock — the gateway's
						     PATH says nothing about per-project tools. -->
						<LspConfigEditor bind:rows={lspRows} showInstalled={false} />
					{:else}
						<div class="skel-rows" aria-hidden="true">
							<Skeleton width="100%" height="52px" radius="var(--radius-md)" />
							<Skeleton width="100%" height="52px" radius="var(--radius-md)" />
							<Skeleton width="100%" height="52px" radius="var(--radius-md)" />
						</div>
					{/if}
				</div>

				<!-- Format checklist -->
				<div class="gsect">
					<div class="gsect-head">
						<h2>格式化</h2>
						<p class="gsect-desc">
							edit/write 后的自动格式化。清单语义同语言服务器——勾选启用、禁用写墓碑，command
							仅对已安装的可改。
						</p>
					</div>
					{#if fmtView}
						<FormatConfigEditor bind:rows={fmtRows} bind:mode={fmtMode} showInstalled={false} />
					{:else}
						<div class="skel-rows" aria-hidden="true">
							<Skeleton width="100%" height="52px" radius="var(--radius-md)" />
							<Skeleton width="100%" height="52px" radius="var(--radius-md)" />
							<Skeleton width="100%" height="52px" radius="var(--radius-md)" />
						</div>
					{/if}
				</div>
			</section>
		{/if}

		<SaveBar {dirtyCount} {saving} onsave={saveAll} ondiscard={discardAll} />
	</div>
</div>

<Toasts />

<style>
	.page {
		height: 100%;
		overflow-y: auto;
	}

	.page-inner {
		padding: var(--space-8) var(--space-10);
		max-width: 960px;
	}

	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: var(--space-6);
		padding-bottom: var(--space-4);
		border-bottom: 1px solid var(--border-subtle);
	}

	h1 {
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.01em;
	}

	.tabs {
		position: relative;
		display: inline-flex;
		gap: 2px;
		background: var(--canvas-float);
		padding: 2px;
		border-radius: var(--radius-md);
	}

	.tab-pill {
		position: absolute;
		top: 2px;
		bottom: 2px;
		left: 0;
		background: var(--canvas-raised);
		border-radius: var(--radius-sm);
		transition:
			transform var(--dur-std) var(--ease-out),
			width var(--dur-std) var(--ease-out);
	}

	.tab {
		position: relative;
		z-index: 1;
		padding: 5px var(--space-3);
		border: none;
		background: transparent;
		color: var(--text-secondary);
		font-size: 13px;
		font-weight: 500;
		border-radius: var(--radius-sm);
		cursor: pointer;
		transition: color var(--dur-fast) var(--ease-out);
	}

	.tab.active {
		color: var(--text-primary);
	}

	.error {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		padding: var(--space-3) var(--space-4);
		border-radius: var(--radius-md);
		border: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
		margin-bottom: var(--space-4);
		font-size: 13px;
	}

	.muted {
		color: var(--text-tertiary);
		font-size: 13px;
		padding: var(--space-4);
	}

	.stack {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}

	.sect {
		margin-top: var(--space-2);
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

	.key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.in {
		width: 100%;
		padding: 6px 8px;
		background: var(--canvas-base);
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

	.ta {
		resize: vertical;
		line-height: 1.5;
	}

	.hint {
		font-size: 11px;
		color: var(--text-tertiary);
		padding: var(--space-1) 0;
	}

	.actions {
		display: flex;
		gap: var(--space-2);
		justify-content: flex-end;
		align-items: center;
	}

	.btn-ghost {
		padding: 5px var(--space-3);
		background: transparent;
		color: var(--text-secondary);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		font-size: 12px;
		cursor: pointer;
		transition: all var(--dur-fast) var(--ease-out);
	}

	.btn-ghost:hover {
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.btn-ghost.danger:hover {
		color: var(--state-error-text);
		border-color: var(--state-error-text);
	}

	.btn-ghost.sm {
		padding: 2px 7px;
	}

	.btn-ghost.full {
		width: 100%;
		margin-bottom: var(--space-2);
	}

	.two-col {
		display: grid;
		grid-template-columns: 240px 1fr;
		gap: var(--space-4);
		align-items: start;
	}

	.list {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.list-row {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		border-radius: var(--radius-sm);
	}

	.list-row.active {
		background: var(--surface-hover);
	}

	.list-btn {
		flex: 1;
		text-align: left;
		background: transparent;
		border: none;
		padding: var(--space-2);
		cursor: pointer;
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}

	.list-name {
		font-size: 12.5px;
		font-weight: 510;
		color: var(--text-primary);
	}

	.list-desc {
		font-size: 10.5px;
		color: var(--text-tertiary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.editor {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		padding: var(--space-4);
	}

	/* ---- 全局设置 tab: flat sections ----
	   One container per concern (permission floor / LSP / format), a quiet
	   heading + one-line desc, then the content directly — no nested tier
	   cards (the checklist rows carry their own per-row source badges). */
	.gsect {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		padding-bottom: var(--space-5);
		border-bottom: 1px solid var(--border-subtle);
	}

	.gsect:last-child {
		border-bottom: none;
		padding-bottom: 0;
	}

	.gsect-head h2 {
		font-family: var(--font-chinese);
		font-size: 13px;
		font-weight: 600;
		color: var(--text-primary);
	}

	.gsect-desc {
		font-family: var(--font-chinese);
		font-size: 11px;
		line-height: 1.6;
		color: var(--text-tertiary);
		margin-top: 2px;
	}

	/* Loading placeholder: compact rows, same rhythm as the editor's `.rows`. */
	.skel-rows {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	@media (max-width: 768px) {
		.grid2,
		.two-col {
			grid-template-columns: 1fr;
		}
	}
</style>
