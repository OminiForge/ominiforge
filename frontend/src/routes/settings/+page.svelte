<script lang="ts">
	import { onMount } from 'svelte';
	import { client } from '$lib/client';
	import Notice from '$lib/components/Notice.svelte';
	import PermissionRulesEditor from '$lib/components/PermissionRulesEditor.svelte';
	import SaveBar from '$lib/components/SaveBar.svelte';
	import Skeleton from '$lib/components/Skeleton.svelte';
	import { resolveEffective, ruleToRow, summaryOf, type Tier } from '$lib/permission-rules';
	import type { ProviderConfig } from '$lib/types/ProviderConfig';
	import type { ProviderType } from '$lib/types/ProviderType';
	import type { ModelConfig } from '$lib/types/ModelConfig';
	import type { Profile } from '$lib/types/Profile';
	import type { ProfileSummary } from '$lib/types/ProfileSummary';
	import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
	import type { ToolInfo } from '$lib/types/ToolInfo';
	import type { WorkspaceSummary } from '$lib/types/WorkspaceSummary';
	import type { WorkspaceConfig } from '$lib/types/WorkspaceConfig';

	type Tab = 'providers' | 'profiles' | 'permissions';
	let tab = $state<Tab>('providers');

	// ---- Providers state ----
	let providers = $state<ProviderConfig[]>([]);
	// Provider names that already have a stored key (from the secret store).
	let configured = $state<Set<string>>(new Set());
	// Per-provider pending key input; only non-empty entries are written on save.
	let keyInput = $state<Record<string, string>>({});
	let providersSnapshot = $state('');

	// ---- Profiles state ----
	let profileList = $state<ProfileSummary[]>([]);
	let selectedName = $state<string>('');
	let profile = $state<Profile | null>(null);
	let profileSnapshot = $state<string | null>(null);

	// ---- Permissions tab state ----
	// The three gate tiers (doc/permission.md §3.1): gateway baseline (bottom),
	// profile, workspace (top). Loaded lazily on first tab open so the
	// providers/profiles path pays nothing.
	let gatewayPolicy = $state<PermissionPolicy | null>(null);
	let gatewaySnapshot = $state<string | null>(null);
	let wsList = $state<WorkspaceSummary[]>([]);
	let selectedWs = $state<string>('');
	let wsConfig = $state<WorkspaceConfig | null>(null);
	let wsTools = $state<ToolInfo[]>([]);
	let wsSnapshot = $state<string | null>(null);
	let permLoaded = $state(false);

	// Tool catalog for the rule editors' tool pickers. Loaded once with
	// providers/profiles; the workspace tier swaps in its own catalog (built-ins
	// + MCP tools) when a workspace is selected.
	let toolCatalog = $state<ToolInfo[]>([]);

	// ---- Shared UI ----
	let loading = $state(true);
	let saving = $state(false);
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

	const PROVIDER_TYPES: ProviderType[] = [
		'openai-chat',
		'openai-completion',
		'anthropic',
		'custom'
	];

	// ---- Dirty tracking: one JSON snapshot per independently-savable unit ----
	const providersDirty = $derived(
		!loading &&
			(JSON.stringify(providers) !== providersSnapshot ||
				Object.values(keyInput).some((k) => k.trim().length > 0))
	);
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
	const wsDirty = $derived(
		wsConfig !== null && wsSnapshot !== null && JSON.stringify(wsConfig) !== wsSnapshot
	);
	const dirtyCount = $derived(
		[providersDirty, profileDirty, gatewayDirty, wsDirty].filter(Boolean).length
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
		providers = view.providers;
		configured = new Set(view.secret_names);
		keyInput = {};
		providersSnapshot = JSON.stringify(providers);
	}

	async function loadProfiles() {
		profileList = await client.listProfiles();
	}

	async function loadTools() {
		toolCatalog = await client.listTools();
	}

	// Lazy-load the permissions tab's data the first time it is opened.
	$effect(() => {
		if (tab === 'permissions' && !permLoaded) {
			permLoaded = true;
			(async () => {
				const [gw, workspaces] = await Promise.all([
					client.getGatewayPermission(),
					client.listWorkspaces()
				]);
				gatewayPolicy = gw ?? {};
				gatewaySnapshot = JSON.stringify(gatewayPolicy);
				wsList = workspaces;
			})().catch((e) => {
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

	// Each save returns success so saveAll can stop at the first failure.
	async function saveProviders(): Promise<boolean> {
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
			providersSnapshot = JSON.stringify(providers);
			return true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return false;
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
			model: { default: null, fallback: null, temperature: null, max_output_tokens: null },
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

	async function selectWorkspace(id: string) {
		error = null;
		selectedWs = id;
		wsConfig = null;
		wsTools = [];
		wsSnapshot = null;
		if (!id) return;
		try {
			// This workspace's tool catalog: built-ins + its MCP tools (best-effort;
			// MCP failures degrade to built-ins server-side).
			const [cfg, tools] = await Promise.all([
				client.getWorkspaceConfig(id),
				client.listWorkspaceTools(id)
			]);
			// The backend omits empty sections; normalize before binding.
			cfg.permission ??= {};
			cfg.mounts ??= [];
			cfg.network ??= null;
			wsConfig = cfg;
			wsTools = tools;
			wsSnapshot = JSON.stringify(cfg);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function saveWorkspace(): Promise<boolean> {
		if (!wsConfig || !selectedWs) return true;
		try {
			await client.saveWorkspaceConfig(selectedWs, wsConfig);
			wsSnapshot = JSON.stringify(wsConfig);
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
			if (providersDirty && !(await saveProviders())) return;
			if (profileDirty && !(await saveProfile())) return;
			if (gatewayDirty && !(await saveGateway())) return;
			if (wsDirty && !(await saveWorkspace())) return;
			flash('已保存');
		} finally {
			saving = false;
		}
	}

	async function discardAll() {
		error = null;
		if (providersDirty) await loadProviders().catch(() => {});
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
		if (wsDirty && selectedWs) await selectWorkspace(selectedWs);
	}

	// ---- Effective view (read-only, doc/permission.md §3.1) ----
	let effOpen = $state(false);
	const effCatalog = $derived(wsTools.length > 0 ? wsTools : toolCatalog);
	const effective = $derived(
		resolveEffective(gatewayPolicy ?? {}, profile?.permission ?? {}, wsConfig?.permission ?? {})
	);
	// Ask rules on tiers shadowed by a higher tier's ask list never fire
	// (wholesale replacement) — surface that, it is otherwise invisible.
	const shadowedAsks = $derived.by(() => {
		const askTiers = [
			['workspace', wsConfig?.permission] as const,
			['profile', profile?.permission] as const,
			['gateway', gatewayPolicy] as const
		]
			.map(([tier, pol]) => ({ tier: tier as Tier, count: pol?.ask?.length ?? 0 }))
			.filter((t) => t.count > 0);
		const winner = askTiers[0];
		return { winner: winner?.tier ?? null, shadowed: askTiers.slice(1) };
	});

	const TIER_LABEL: Record<Tier, string> = {
		gateway: '网关基线',
		profile: 'Profile',
		workspace: '工作区'
	};

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
				<button
					class="tab"
					class:active={tab === 'permissions'}
					onclick={() => (tab = 'permissions')}
				>
					门控
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
							<input
								class="in title-in"
								placeholder="provider 名称"
								bind:value={p.name}
								spellcheck="false"
							/>
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
								<input
									class="in"
									bind:value={p.base_url}
									placeholder="https://api…/v1"
									spellcheck="false"
								/>
							</label>
							<label class="field">
								<span class="key">api_key_env（回退）</span>
								<input
									class="in"
									bind:value={p.api_key_env}
									placeholder="OPENAI_API_KEY"
									spellcheck="false"
								/>
							</label>
							<label class="field">
								<span class="key">
									API Key
									{#if configured.has(p.name)}<span class="badge ok">已配置</span>{:else}<span
											class="badge">未配置</span
										>{/if}
								</span>
								<div class="key-row">
									<input
										class="in"
										type="password"
										placeholder={configured.has(p.name)
											? '••••••（保存后更新）'
											: '输入 key（保存到 DB）'}
										bind:value={keyInput[p.name]}
										spellcheck="false"
										autocomplete="off"
									/>
									{#if configured.has(p.name)}
										<button class="btn-ghost danger" onclick={() => clearSecret(p.name)}
											>清除</button
										>
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
										<input
											class="in num"
											type="number"
											placeholder="128000"
											bind:value={m.context_window}
										/>
									</label>
									<label class="mfield">
										<span class="mkey">最大输出</span>
										<input
											class="in num"
											type="number"
											placeholder="16384"
											bind:value={m.max_output_tokens}
										/>
									</label>
									<label class="mfield">
										<span class="mkey">默认温度</span>
										<input
											class="in num"
											type="number"
											step="0.1"
											placeholder="0.0"
											bind:value={m.default_temperature}
										/>
									</label>
									<button
										class="btn-ghost danger mrm"
										onclick={() => removeModel(pi, mi)}
										aria-label="删除 model">×</button
									>
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
								<input
									class="in"
									value={pf.profile.extends ?? ''}
									oninput={(e) => (pf.profile.extends = e.currentTarget.value || null)}
									placeholder="父 profile"
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
								<span class="key">Model default</span>
								<input
									class="in"
									value={pf.model.default ?? ''}
									oninput={(e) => (pf.model.default = e.currentTarget.value || null)}
									placeholder="provider/model_id"
									spellcheck="false"
								/>
							</label>
							<label class="field">
								<span class="key">Temperature</span>
								<input
									class="in"
									value={pf.model.temperature ?? ''}
									oninput={(e) => (pf.model.temperature = numOrNull(e.currentTarget.value))}
									placeholder="模型默认"
								/>
							</label>
							<label class="field">
								<span class="key">Max output tokens</span>
								<input
									class="in"
									value={pf.model.max_output_tokens ?? ''}
									oninput={(e) => (pf.model.max_output_tokens = numOrNull(e.currentTarget.value))}
									placeholder="模型默认"
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
					{:else}
						<p class="muted">选择左侧 profile 编辑，或新建一个。</p>
					{/if}
				</div>
			</section>
		{:else}
			<!-- PERMISSIONS-SECTION -->
			<section class="stack">
				<div class="tier">
					<div class="tier-head">
						<h2>Gateway 基线</h2>
						<p class="tier-desc">
							三层解析的最低层。这里的 deny 是整个网关的安全底线，任何 profile / 工作区都无法放开。
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
						<!-- Mirror the loaded layout (defaults table + rule rows) so
						     content doesn't jump when it lands (§3.2). -->
						<div class="skel-rows" aria-hidden="true">
							<Skeleton width="100%" height="38px" radius="var(--radius-md)" />
							<Skeleton width="100%" height="28px" />
							<Skeleton width="64%" height="28px" />
						</div>
					{/if}
				</div>

				<div class="tier">
					<div class="tier-head">
						<h2>Profile</h2>
						<p class="tier-desc">中间层。只列本层添加的规则；空的 profile 不施加任何门控。</p>
					</div>
					<select
						class="in sel tier-picker"
						value={selectedName}
						onchange={(e) => selectProfile(e.currentTarget.value)}
					>
						<option value="">选择 profile…</option>
						{#each profileList as p (p.name)}
							<option value={p.name}>{p.name}</option>
						{/each}
					</select>
					{#if profile}
						{#key selectedName}
							<PermissionRulesEditor bind:policy={profile.permission} tools={toolCatalog} />
						{/key}
					{:else if selectedName}
						<div class="skel-rows" aria-hidden="true">
							<Skeleton width="100%" height="28px" />
							<Skeleton width="72%" height="28px" />
							<Skeleton width="48%" height="28px" />
						</div>
					{/if}
				</div>

				<div class="tier">
					<div class="tier-head">
						<h2>工作区</h2>
						<p class="tier-desc">
							最高层，存于网关可信目录。deny 与下层并集（只增不减），ask 设了则整体替换下层。
						</p>
					</div>
					<select
						class="in sel tier-picker"
						value={selectedWs}
						onchange={(e) => selectWorkspace(e.currentTarget.value)}
					>
						<option value="">选择工作区…</option>
						{#each wsList as w (w.id)}
							<option value={w.id}>{w.path ?? w.id}</option>
						{/each}
					</select>
					{#if wsConfig && wsConfig.permission}
						{#key selectedWs}
							<PermissionRulesEditor bind:policy={wsConfig.permission} tools={wsTools} />
						{/key}
					{:else if selectedWs}
						<div class="skel-rows" aria-hidden="true">
							<Skeleton width="100%" height="28px" />
							<Skeleton width="72%" height="28px" />
							<Skeleton width="48%" height="28px" />
						</div>
					{/if}
				</div>

				<div class="tier">
					<h2 class="eff-title">
						<button class="eff-head" onclick={() => (effOpen = !effOpen)} aria-expanded={effOpen}>
							<svg
								class="chev"
								class:open={effOpen}
								viewBox="0 0 14 14"
								fill="none"
								stroke="currentColor"
								stroke-width="1.6"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<polyline points="5,3 9,7 5,11" />
							</svg>
							生效结果
							<span class="eff-count">
								{effective.length > 0 ? `${effective.length} 条生效规则` : '全部允许'}
							</span>
						</button>
					</h2>
					{#if effOpen}
						<div class="eff-body">
							<p class="tier-desc">
								按当前选中的 profile 与工作区计算：deny 三层并集，ask 取最高设置层，allow
								三层并集且命中时豁免 ask。
							</p>
							{#if effective.length === 0}
								<p class="muted">三层都没有规则 —— 所有工具直接允许。</p>
							{:else}
								{#each effective as eff, i (i)}
									{@const row = ruleToRow(eff.rule, eff.list)}
									<div class="eff-row">
										<span
											class="verdict"
											class:deny={eff.list === 'deny'}
											class:ask={eff.list === 'ask'}
											class:allow={eff.list === 'allow'}
										>
											{eff.list === 'deny' ? '拒绝' : eff.list === 'allow' ? '允许' : '询问'}
										</span>
										<span class="eff-summary">
											{row ? summaryOf(row, effCatalog) : JSON.stringify(eff.rule)}
										</span>
										<span class="tier-badge">{TIER_LABEL[eff.tier]}</span>
									</div>
								{/each}
							{/if}
							{#if shadowedAsks.shadowed.length > 0}
								<p class="eff-note">
									{TIER_LABEL[shadowedAsks.winner ?? 'gateway']} 层设了询问规则，
									{shadowedAsks.shadowed
										.map((s) => `${TIER_LABEL[s.tier]} 层的 ${s.count} 条`)
										.join('、')}
									询问规则被整表替换，不生效。
								</p>
							{/if}
						</div>
					{/if}
				</div>
			</section>
		{/if}

		<SaveBar {dirtyCount} {saving} onsave={saveAll} ondiscard={discardAll} />
	</div>
</div>

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

	/* ---- Permissions tab: tier sections ---- */
	.tier {
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		padding: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.tier-head h2 {
		font-family: var(--font-chinese);
		font-size: 13px;
		font-weight: 600;
		color: var(--text-primary);
	}

	.tier-desc {
		font-family: var(--font-chinese);
		font-size: 11px;
		line-height: 1.6;
		color: var(--text-tertiary);
		margin-top: 2px;
	}

	.tier-picker {
		max-width: 320px;
	}

	/* Loading placeholder: compact rows, same rhythm as the editor's `.rows`. */
	.skel-rows {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.sel {
		font-family: var(--font-chinese);
		background: var(--canvas-raised);
	}

	/* ---- Effective view ---- */
	/* Heading wraps the toggle (button is valid phrasing content inside h2; the
	   inverse nesting is not). The button inherits this font via the global
	   `button { font: inherit }` reset. */
	.eff-title {
		font-family: var(--font-chinese);
		font-size: 13px;
		font-weight: 600;
		color: var(--text-primary);
	}

	.eff-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		text-align: left;
	}

	.chev {
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-fast) var(--ease-out);
	}

	.chev.open {
		transform: rotate(90deg);
	}

	.eff-count {
		margin-left: auto;
		font-family: var(--font-chinese);
		font-size: 11px;
		color: var(--text-tertiary);
	}

	.eff-body {
		display: flex;
		flex-direction: column;
		gap: 2px;
		border-top: 1px solid var(--border-subtle);
		padding-top: var(--space-3);
	}

	.eff-row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 4px 0;
	}

	.verdict {
		flex-shrink: 0;
		font-family: var(--font-chinese);
		font-size: 11px;
		font-weight: 590;
		padding: 1px 6px;
		border-radius: var(--radius-sm);
	}

	.verdict.deny {
		color: var(--state-error-text);
		background: var(--state-error-bg);
	}

	.verdict.allow {
		color: var(--state-done-text);
		background: var(--state-done-bg);
	}

	.verdict.ask {
		color: var(--state-running-text);
		background: var(--state-running-bg);
	}

	.eff-summary {
		flex: 1;
		min-width: 0;
		font-family: var(--font-chinese);
		font-size: 12px;
		color: var(--text-secondary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.tier-badge {
		flex-shrink: 0;
		font-family: var(--font-chinese);
		font-size: 11px;
		color: var(--text-tertiary);
		background: var(--canvas-float);
		padding: 1px 6px;
		border-radius: var(--radius-sm);
	}

	.eff-note {
		margin-top: var(--space-2);
		font-family: var(--font-chinese);
		font-size: 11px;
		line-height: 1.6;
		color: var(--state-running-text);
	}

	@media (max-width: 768px) {
		.grid2,
		.two-col {
			grid-template-columns: 1fr;
		}
	}
</style>
