<script lang="ts">
	import { onMount } from 'svelte';
	import { client } from '$lib/client';
	import Notice from '$lib/components/Notice.svelte';
	import PermissionEditor from '$lib/components/PermissionEditor.svelte';
	import Skeleton from '$lib/components/Skeleton.svelte';
	import type { ProviderConfig } from '$lib/types/ProviderConfig';
	import type { ProviderType } from '$lib/types/ProviderType';
	import type { ModelConfig } from '$lib/types/ModelConfig';
	import type { Profile } from '$lib/types/Profile';
	import type { ProfileSummary } from '$lib/types/ProfileSummary';
	import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
	import type { ToolInfo } from '$lib/types/ToolInfo';

	type Tab = 'providers' | 'profiles' | 'gateway';
	let tab = $state<Tab>('providers');

	// ---- Providers state ----
	let providers = $state<ProviderConfig[]>([]);
	// Provider names that already have a stored key (from the secret store).
	let configured = $state<Set<string>>(new Set());
	// Per-provider pending key input; only non-empty entries are written on save.
	let keyInput = $state<Record<string, string>>({});

	// ---- Profiles state ----
	let profileList = $state<ProfileSummary[]>([]);
	let selectedName = $state<string>('');
	let profile = $state<Profile | null>(null);

	// ---- Gateway state ----
	// The gateway-wide baseline permission policy (bottom tier). Loaded lazily on
	// first Gateway-tab open so the providers/profiles path pays nothing.
	let gatewayPermission = $state<PermissionPolicy | null>(null);

	// Tool catalog for the permission editors' per-tool cards. Loaded once with
	// providers/profiles; empty until then (editor renders no cards, harmless).
	let toolCatalog = $state<ToolInfo[]>([]);

	// ---- Shared UI ----
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);

	// Sliding pill behind the active tab: measured from the real button so it
	// tracks text width/font loading instead of hardcoded geometry.
	let tabsEl = $state<HTMLDivElement | null>(null);
	let pill = $state({ x: 0, w: 0 });
	$effect(() => {
		void tab; // re-measure on every tab switch
		const active = tabsEl?.querySelector<HTMLElement>('.tab.active');
		if (active) pill = { x: active.offsetLeft, w: active.offsetWidth };
	});

	const PROVIDER_TYPES: ProviderType[] = ['openai-chat', 'openai-completion', 'anthropic', 'custom'];

	async function loadProviders() {
		const view = await client.getProviders();
		providers = view.providers;
		configured = new Set(view.secret_names);
		keyInput = {};
	}

	async function loadProfiles() {
		profileList = await client.listProfiles();
	}

	async function loadTools() {
		toolCatalog = await client.listTools();
	}

	async function loadGatewayPermission() {
		gatewayPermission = await client.getGatewayPermission();
	}

	async function saveGatewayPermission() {
		if (!gatewayPermission) return;
		error = null;
		try {
			await client.saveGatewayPermission(gatewayPermission);
			flash('Gateway 门控已保存');
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// Lazy-load the gateway policy the first time its tab is opened.
	$effect(() => {
		if (tab === 'gateway' && gatewayPermission === null) {
			loadGatewayPermission().catch((e) => {
				error = e instanceof Error ? e.message : String(e);
			});
		}
	});

	onMount(async () => {
		try {
			await Promise.all([loadProviders(), loadProfiles(), loadTools()]);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	});

	function flash(msg: string) {
		notice = msg;
		setTimeout(() => (notice = null), 2500);
	}

	// ---- Provider editing ----
	function addProvider() {
		providers = [
			...providers,
			{ name: '', type: 'openai-chat', base_url: '', api_key_env: '', models: [] }
		];
	}

	function removeProvider(i: number) {
		providers = providers.filter((_, idx) => idx !== i);
	}

	function addModel(pi: number) {
		const m: ModelConfig = {
			id: '',
			context_window: 128000,
			max_output_tokens: 16384,
			default_temperature: 0,
			pricing: null
		};
		providers[pi].models = [...providers[pi].models, m];
	}

	function removeModel(pi: number, mi: number) {
		providers[pi].models = providers[pi].models.filter((_, idx) => idx !== mi);
	}

	async function saveProviders() {
		error = null;
		try {
			await client.saveProviders({ providers });
			// Persist any newly-entered API keys, then clear the inputs.
			for (const [name, key] of Object.entries(keyInput)) {
				if (key.trim()) {
					await client.setSecret(name, key.trim());
					configured = new Set([...configured, name]);
				}
			}
			keyInput = {};
			flash('Providers saved');
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function clearSecret(name: string) {
		error = null;
		try {
			await client.deleteSecret(name);
			const next = new Set(configured);
			next.delete(name);
			configured = next;
			flash(`Key for ${name} removed`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// ---- Profile editing ----
	async function selectProfile(name: string) {
		error = null;
		selectedName = name;
		try {
			profile = await client.getProfile(name);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			profile = null;
		}
	}

	function newProfile() {
		selectedName = '';
		profile = {
			profile: { name: '', description: null, extends: null },
			prompt: { system: null, system_file: null },
			model: { default: null, fallback: null, temperature: null, max_output_tokens: null },
			tools: { builtin: null, mcp_servers: [], disabled: [] },
			context: { compaction_threshold: null, compaction_model: null, injection_max_tokens: null },
			skills: { enabled: [] },
			memory: { scopes: [], auto_write: null },
			budget: { session_max_usd: null, daily_max_usd: null, warn_at_percent: null },
			hooks: { before_tool: [], after_tool: [] },
			network: { policy: null, allow: [] },
			permission: { deny: [], ask: [] }
		};
	}

	async function saveProfile() {
		if (!profile) return;
		error = null;
		const name = profile.profile.name.trim();
		if (!name) {
			error = 'Profile name is required';
			return;
		}
		try {
			await client.saveProfile(name, profile);
			await loadProfiles();
			selectedName = name;
			flash(`Profile ${name} saved`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
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
			}
			flash(`Profile ${name} deleted`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// Bridge a nullable-number model field to a text input (empty = null).
	function numOrNull(v: string): number | null {
		const t = v.trim();
		return t === '' ? null : Number(t);
	}
</script>

<!-- TEMPLATE-APPEND -->

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
				<button class="tab" class:active={tab === 'gateway'} onclick={() => (tab = 'gateway')}>
					Gateway
				</button>
			</div>
		</header>

		{#if notice}<Notice message={notice} />{/if}
		{#if error}<p class="error">{error}</p>{/if}

		{#if loading}
			<p class="muted">加载中…</p>
		{:else if tab === 'providers'}
			<!-- PROVIDERS-SECTION -->
			<section class="stack">
				{#each providers as p, pi (pi)}
					<div class="panel">
						<div class="panel-head">
							<input class="in title-in" placeholder="provider 名称" bind:value={p.name} spellcheck="false" />
							<button class="btn-ghost danger" onclick={() => removeProvider(pi)}>删除</button>
						</div>

						<div class="grid2">
							<label class="field">
								<span class="key">Type</span>
								<select class="in" bind:value={p.type}>
									{#each PROVIDER_TYPES as t (t)}<option value={t}>{t}</option>{/each}
								</select>
							</label>
							<label class="field">
								<span class="key">Base URL</span>
								<input class="in" bind:value={p.base_url} placeholder="https://api…/v1" spellcheck="false" />
							</label>
							<label class="field">
								<span class="key">api_key_env（回退）</span>
								<input class="in" bind:value={p.api_key_env} placeholder="OPENAI_API_KEY" spellcheck="false" />
							</label>
							<label class="field">
								<span class="key">
									API Key
									{#if configured.has(p.name)}<span class="badge ok">已配置</span>{:else}<span class="badge">未配置</span>{/if}
								</span>
								<div class="key-row">
									<input
										class="in"
										type="password"
										placeholder={configured.has(p.name) ? '••••••（保存后更新）' : '输入 key（保存到 DB）'}
										bind:value={keyInput[p.name]}
										spellcheck="false"
										autocomplete="off"
									/>
									{#if configured.has(p.name)}
										<button class="btn-ghost danger" onclick={() => clearSecret(p.name)}>清除</button>
									{/if}
								</div>
							</label>
						</div>

						<div class="models">
							<div class="models-head">
								<span class="key">Models</span>
								<button class="btn-ghost" onclick={() => addModel(pi)}>+ model</button>
							</div>
							{#each p.models as m, mi (mi)}
								<div class="model-row">
									<label class="mfield grow">
										<span class="mkey">Model ID</span>
										<input class="in" placeholder="gpt-4o" bind:value={m.id} spellcheck="false" />
									</label>
									<label class="mfield">
										<span class="mkey">上下文窗口</span>
										<input class="in num" type="number" placeholder="128000" bind:value={m.context_window} />
									</label>
									<label class="mfield">
										<span class="mkey">最大输出</span>
										<input class="in num" type="number" placeholder="16384" bind:value={m.max_output_tokens} />
									</label>
									<label class="mfield">
										<span class="mkey">默认温度</span>
										<input class="in num" type="number" step="0.1" placeholder="0.0" bind:value={m.default_temperature} />
									</label>
									<button class="btn-ghost danger mrm" onclick={() => removeModel(pi, mi)} aria-label="删除 model">×</button>
								</div>
							{/each}
							{#if p.models.length === 0}
								<p class="hint">还没有 model，点 “+ model” 添加。</p>
							{/if}
						</div>
					</div>
				{/each}

				<div class="actions">
					<button class="btn-ghost" onclick={addProvider}>+ provider</button>
					<button class="btn-primary" onclick={saveProviders}>保存 Providers</button>
				</div>
			</section>
		{:else if tab === 'profiles'}
			<!-- PROFILES-SECTION -->
			<section class="two-col">
				<aside class="list">
					<button class="btn-ghost full" onclick={newProfile}>+ 新 profile</button>
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
								<input class="in" value={pf.profile.extends ?? ''} oninput={(e) => (pf.profile.extends = e.currentTarget.value || null)} placeholder="父 profile" spellcheck="false" />
							</label>
							<label class="field span2">
								<span class="key">Description</span>
								<input class="in" value={pf.profile.description ?? ''} oninput={(e) => (pf.profile.description = e.currentTarget.value || null)} />
							</label>
						</div>

						<label class="field">
							<span class="key">System Prompt</span>
							<textarea class="in ta" rows="4" value={pf.prompt.system ?? ''} oninput={(e) => (pf.prompt.system = e.currentTarget.value || null)}></textarea>
						</label>

						<div class="grid2">
							<label class="field">
								<span class="key">Model default</span>
								<input class="in" value={pf.model.default ?? ''} oninput={(e) => (pf.model.default = e.currentTarget.value || null)} placeholder="provider/model_id" spellcheck="false" />
							</label>
							<label class="field">
								<span class="key">Temperature</span>
								<input class="in" value={pf.model.temperature ?? ''} oninput={(e) => (pf.model.temperature = numOrNull(e.currentTarget.value))} placeholder="模型默认" />
							</label>
							<label class="field">
								<span class="key">Max output tokens</span>
								<input class="in" value={pf.model.max_output_tokens ?? ''} oninput={(e) => (pf.model.max_output_tokens = numOrNull(e.currentTarget.value))} placeholder="模型默认" />
							</label>
							<label class="field">
								<span class="key">Compaction threshold</span>
								<input class="in" value={pf.context.compaction_threshold ?? ''} oninput={(e) => (pf.context.compaction_threshold = numOrNull(e.currentTarget.value))} placeholder="0.8" />
							</label>
						</div>

						<div class="perm-block">
							<div class="perm-head">
								<span class="key">Permission 门控</span>
								<span class="perm-note">profile 层（三层解析的中间层，见 doc/permission.md §3）</span>
							</div>
							<PermissionEditor bind:policy={pf.permission} tools={toolCatalog} />
						</div>

						<div class="actions">
							<button class="btn-primary" onclick={saveProfile}>保存 Profile</button>
						</div>
					{:else}
						<p class="muted">选择左侧 profile 编辑，或新建一个。</p>
					{/if}
				</div>
			</section>
		{:else}
			<!-- GATEWAY-SECTION -->
			<section class="stack">
				<div class="editor">
					<div class="perm-head">
						<span class="key">Gateway 基线门控</span>
						<span class="perm-note">
							三层解析的最低层（doc/permission.md §3.1）。deny 规则是全 gateway 的安全底线，任何
							profile / workspace 都无法放开；对<strong>新建会话</strong>立即生效，并持久化到 gateway.toml。
						</span>
					</div>
					{#if gatewayPermission}
						<PermissionEditor bind:policy={gatewayPermission} tools={toolCatalog} />
						<div class="actions">
							<button class="btn-primary" onclick={saveGatewayPermission}>保存 Gateway 门控</button>
						</div>
					{:else}
						<div class="skel-cards" aria-hidden="true">
							{#each Array(4) as _}
								<Skeleton width="100%" height="64px" radius="var(--radius-md)" />
							{/each}
						</div>
					{/if}
				</div>
			</section>
		{/if}
	</div>
</div>

<!-- STYLE-APPEND -->
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
		padding: var(--space-8);
		text-align: center;
	}

	.stack {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}
	/* STYLE-APPEND-2 */
	.panel {
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		padding: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.panel-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
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

	.title-in {
		flex: 1;
		font-weight: 600;
		font-size: 13px;
	}

	.ta {
		resize: vertical;
		line-height: 1.5;
	}

	.num {
		max-width: 90px;
	}

	.key-row {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}

	.badge {
		font-family: var(--font-mono);
		font-size: 9px;
		padding: 1px 5px;
		border-radius: 3px;
		background: var(--canvas-float);
		color: var(--text-tertiary);
		text-transform: none;
		letter-spacing: 0;
	}

	.badge.ok {
		color: var(--state-done-text);
		background: var(--state-done-bg);
	}
	/* STYLE-APPEND-3 */
	.models {
		border-top: 1px solid var(--border-subtle);
		padding-top: var(--space-3);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.models-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}

	.model-row {
		display: flex;
		gap: var(--space-2);
		align-items: flex-end;
	}

	.mfield {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}

	.mfield.grow {
		flex: 1;
	}

	.mkey {
		font-family: var(--font-mono);
		font-size: 9px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}

	.mrm {
		margin-bottom: 1px;
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

	.btn-primary {
		padding: 7px var(--space-4);
		background: var(--accent);
		color: var(--accent-fg);
		border: 1px solid var(--accent);
		border-radius: var(--radius-sm);
		font-size: 13px;
		font-weight: 590;
		cursor: pointer;
		transition: background var(--motion-fast);
	}

	.btn-primary:hover {
		background: var(--accent-hover);
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

	.perm-block {
		border-top: 1px solid var(--border-subtle);
		padding-top: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.perm-head {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
	}

	.perm-note {
		font-size: 11px;
		color: var(--text-tertiary);
	}

	.skel-cards {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	@media (max-width: 768px) {
		.grid2,
		.two-col {
			grid-template-columns: 1fr;
		}
	}


</style>


