<script lang="ts">
	import { onDestroy } from 'svelte';
	import { browser } from '$app/environment';
	import { page } from '$app/state';
	import { goto, replaceState } from '$app/navigation';
	import { marked } from 'marked';
	import DOMPurify from 'dompurify';
	import { client } from '$lib/client';
	import type { EventSubscription } from '$lib/client-core';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import { apply, emptyState, type ConversationState, type Item } from '$lib/conversation';
	import { currentSession, currentRuntime, currentRuntimeModels } from '$lib/stores/currentSession';

	/** Sentinel id for a not-yet-created (draft) session. Reaching `/sessions/new`
	 *  shows an empty conversation; the real session is created lazily on the
	 *  first send, so merely opening a draft never litters the store with empty
	 *  sessions. The backend never mints `new` as a real id, so it can't clash. */
	const DRAFT_ID = 'new';

	/** When the user is within this many pixels from the bottom we consider
	 *  them "at the bottom" and auto-scroll on new content. This tolerance
	 *  avoids missing the trigger due to sub-pixel rounding or small
	 *  layout shifts. */
	const SCROLL_BOTTOM_THRESHOLD = 80;

	let convo = $state<ConversationState>(emptyState());
	let input = $state('');
	let sending = $state(false);
	let error = $state<string | null>(null);
	let sessionId = $state(page.params.id!);
	let meta = $state<SessionMeta | null>(null);
	let sub: EventSubscription | undefined;
	let streamEl = $state<HTMLElement | null>(null);
	// Whether the user is scrolled to (or near) the bottom – controls auto-scroll.
	let shouldAutoScroll = $state(true);
	// Track collapsed state for reasoning items separately (index → collapsed)
	let collapsed = $state<Record<number, boolean>>({});

	const isDraft = $derived(sessionId === DRAFT_ID);

	/** Returns `true` when the element is scrolled close enough to the bottom. */
	function isNearBottom(el: HTMLElement): boolean {
		return el.scrollHeight - el.scrollTop - el.clientHeight < SCROLL_BOTTOM_THRESHOLD;
	}

	/** Called whenever the user scrolls inside the stream container.
	 *  Updates `shouldAutoScroll` so that we only auto-scroll when the
	 *  user hasn't deliberately scrolled up to read history. */
	function onStreamScroll() {
		if (streamEl) {
			shouldAutoScroll = isNearBottom(streamEl);
		}
	}

	/** Load session meta for the RUNTIME sidebar panel. Best-effort: a failure
	 *  here must not break the conversation view, so errors are swallowed. */
	async function loadMeta(id: string) {
		try {
			meta = await client.getSession(id);
			currentSession.set(meta);
		} catch {
			/* RUNTIME panel just stays empty; conversation still works. */
		}
		// Resolve the config-layer model independently: a runtime failure must not
		// blank the meta we just loaded, and a stale value must not linger, so it
		// owns its own try/clear.
		try {
			currentRuntime.set(await client.getRuntime(id));
		} catch {
			currentRuntime.set(null);
		}
	}

	function subscribe(id: string) {
		sub?.close();
		convo = emptyState();
		collapsed = {};
		// A new subscription means fresh content – start auto-scrolling.
		shouldAutoScroll = true;
		sub = client.subscribeEvents(id, {
			onEvent: (ev) => {
				convo = apply(convo, ev);
				if (ev.type === 'compacted') {
					sessionId = ev.new_session_id;
					subscribe(ev.new_session_id);
					void loadMeta(ev.new_session_id);
				}
				// Only auto-scroll when the user is already at (or near) the
				// bottom of the stream. This prevents yanking them back to the
				// latest message while they're reading history.
				if (shouldAutoScroll) {
					requestAnimationFrame(() => {
						streamEl?.scrollTo({ top: streamEl.scrollHeight, behavior: 'smooth' });
					});
				}
			},
			onError: (e) => {
				error = e instanceof Error ? e.message : String(e);
			}
		});
	}

	$effect(() => {
		const id = sessionId;
		// Draft: show an empty conversation, don't subscribe or load meta. The
		// real session doesn't exist yet — it's created on the first send().
		if (id === DRAFT_ID) {
			sub?.close();
			convo = emptyState();
			collapsed = {};
			currentSession.set(null);
			currentRuntime.set(null);
			currentRuntimeModels.set([]);
			return;
		}
		subscribe(id);
		void loadMeta(id);
	});

	// Mirror the runtime-layer models the fold collected into the store the
	// sidebar RUNTIME panel reads, so it can flag divergence from the configured
	// model (B4). Reactive on convo so it tracks each new RequestStarted.
	$effect(() => {
		currentRuntimeModels.set([...convo.runtimeModels]);
	});

	onDestroy(() => {
		sub?.close();
		// Clear so the sidebar RUNTIME panel doesn't leak this session's context
		// onto the list / monitor / evolution pages.
		currentSession.set(null);
		currentRuntime.set(null);
		currentRuntimeModels.set([]);
	});

	async function send() {
		const text = input.trim();
		if (!text || sending) return;
		sending = true;
		error = null;
		try {
			if (sessionId === DRAFT_ID) {
				// Lazily create the real session on first send, then adopt its id.
				// Setting sessionId drives the $effect to subscribe + load meta;
				// replaceState swaps the URL without pushing a history entry (so
				// Back doesn't return to the empty draft).
				const realId = await client.createSession();
				sessionId = realId;
				replaceState(`/sessions/${realId}`, {});
				await client.sendMessage(realId, text);
			} else {
				await client.sendMessage(sessionId, text);
			}
			input = '';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			sending = false;
		}
	}

	async function cancel() {
		try {
			await client.cancel(sessionId);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// Compaction stays available programmatically; the button was removed in
	// favour of a future `/` command (see redesign plan). Keep the function so
	// the slash-command wiring can call it later.
	// eslint-disable-next-line @typescript-eslint/no-unused-vars
	async function compact() {
		try {
			await client.compact(sessionId);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter' && !e.shiftKey) {
			e.preventDefault();
			void send();
		}
	}

	function renderMarkdown(text: string): string {
		if (!browser) return escapeHtml(text);
		const raw = marked.parse(text, { async: false }) as string;
		return DOMPurify.sanitize(raw);
	}

	function escapeHtml(s: string): string {
		return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	}

	function toggleCollapse(item: Item, i: number) {
		// Flip from the currently *displayed* state, not the raw map value.
		// Auto-collapsed items have no map entry yet, so `!collapsed[i]` would
		// compute `true` (= still collapsed) on first click, needing a second
		// click to take effect. Seed from isCollapsed so one click always flips.
		collapsed = { ...collapsed, [i]: !isCollapsed(item, i) };
	}

	function isCollapsed(item: Item, i: number): boolean {
		if (i in collapsed) return collapsed[i];
		// Auto-collapse finished reasoning and completed tools
		if (item.kind === 'reasoning') return !item.streaming;
		if (item.kind === 'tool') return item.status === 'done' || item.status === 'error';
		return false;
	}

	function shortPreview(text: string): string {
		const first = text.split('\n')[0].slice(0, 60);
		return first.length < text.split('\n')[0].length ? first + '…' : first;
	}

	/** One-line summary of a tool call's arguments, shown when the tool is
	 *  collapsed so the user sees *what* ran, not just the tool name. Pulls the
	 *  most meaningful field (command/path/query/…) from the parsed JSON args,
	 *  falling back to the raw args string. */
	function toolPreview(args: string): string {
		if (!args || args === '{}') return '';
		let parsed: unknown;
		try {
			parsed = JSON.parse(args);
		} catch {
			return clip(args, 80);
		}
		if (parsed && typeof parsed === 'object') {
			const obj = parsed as Record<string, unknown>;
			const keys = ['command', 'cmd', 'script', 'path', 'file', 'file_path', 'query', 'url', 'pattern'];
			for (const k of keys) {
				if (typeof obj[k] === 'string' && obj[k]) return clip(obj[k] as string, 80);
			}
			// No known key: show first string value, else compact JSON.
			const firstStr = Object.values(obj).find((v) => typeof v === 'string' && v);
			if (typeof firstStr === 'string') return clip(firstStr, 80);
		}
		return clip(args, 80);
	}

	function clip(s: string, n: number): string {
		const line = s.split('\n')[0];
		return line.length > n ? line.slice(0, n) + '…' : line;
	}

	/** Pretty-print + syntax-highlight tool-call JSON args into safe HTML.
	 *  Falls back to escaped raw text when args aren't valid JSON. Highlight
	 *  classes mirror the design tokens (--syntax-key/str/num). */
	function renderArgs(args: string): string {
		let pretty = args;
		try {
			pretty = JSON.stringify(JSON.parse(args), null, 2);
		} catch {
			return escapeHtml(args);
		}
		const esc = escapeHtml(pretty);
		// Keys: "foo": → highlight the key; strings / numbers as values.
		return esc
			.replace(/&quot;([^&]*?)&quot;(\s*:)/g, '<span class="syn-key">&quot;$1&quot;</span>$2')
			.replace(/:\s*&quot;([^&]*?)&quot;/g, ': <span class="syn-str">&quot;$1&quot;</span>')
			.replace(/:\s*(-?\d+(?:\.\d+)?)/g, ': <span class="syn-num">$1</span>');
	}

	/** Short session label for the topbar: prefer the latest user message would
	 *  be ideal, but we don't track titles yet — show a workspace-derived label
	 *  or the session id. */
	function topbarTitle(): string {
		if (sessionId === DRAFT_ID) return 'New session';
		if (meta?.workspace) {
			const parts = meta.workspace.split('/').filter(Boolean);
			return parts[parts.length - 1] ?? sessionId;
		}
		return shortId(sessionId);
	}

	function shortId(id: string): string {
		return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
	}

	const incomplete = $derived(convo.lastSettle != null);
</script>

<div class="conv-page">
	<!-- TOPBAR -->
	<div class="topbar">
		<a href="/sessions" class="topbar-back" title="返回会话列表" aria-label="Back to sessions">
			<svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
				<polyline points="8.5,2.5 4,7 8.5,11.5" />
			</svg>
		</a>
		<span class="topbar-title">{topbarTitle()}</span>
		<div class="topbar-sep"></div>
		<div class="topbar-meta">
			{#if isDraft}
				<span class="mono draft-hint">draft · 发送后创建</span>
			{:else}
				<span class="mono">{shortId(sessionId)}</span>
				{#if incomplete}
					<span class="topbar-badge badge-running">incomplete</span>
				{/if}
			{/if}
		</div>
		{#if !isDraft}
			<div class="topbar-actions">
				<button class="topbar-btn" onclick={cancel}>Cancel</button>
			</div>
		{/if}
	</div>

	{#if error}
		<div class="error-bar">{error}</div>
	{/if}

	<!-- CONVERSATION SCROLL -->
	<div class="conv-scroll" bind:this={streamEl} onscroll={onStreamScroll}>
		<div class="conv-inner">
			{#each convo.items as item, i (i)}
				{#if item.kind === 'user'}
					<div class="item item-user">
						<div class="user-bubble">{item.text}</div>
					</div>

				{:else if item.kind === 'text'}
					{#if item.text.trim()}
						<div class="item item-text" class:streaming={item.streaming}>
							{#if browser}
								<!-- eslint-disable-next-line svelte/no-at-html-tags -->
								{@html renderMarkdown(item.text)}
							{:else}
								{item.text}
							{/if}
						</div>
					{/if}

				{:else if item.kind === 'reasoning'}
					{#if item.text.trim()}
						<div class="item item-reasoning" class:expanded={!isCollapsed(item, i)}>
							<button class="reasoning-toggle" onclick={() => toggleCollapse(item, i)} aria-expanded={!isCollapsed(item, i)}>
								<svg class="reasoning-toggle-icon" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
									<polyline points="5,3 9,7 5,11" />
								</svg>
								<span class="reasoning-label">Thinking</span>
								{#if isCollapsed(item, i)}
									<span class="reasoning-preview">{shortPreview(item.text)}</span>
								{/if}
								{#if item.streaming}
									<span class="streaming-dot"></span>
								{/if}
							</button>
							{#if !isCollapsed(item, i)}
								<div class="reasoning-body">
									{#if browser}
										<!-- eslint-disable-next-line svelte/no-at-html-tags -->
										{@html renderMarkdown(item.text)}
									{:else}
										{item.text}
									{/if}
								</div>
							{/if}
						</div>
					{/if}

				{:else if item.kind === 'tool'}
					<div class="item">
						<div
							class="tool-block"
							class:done={item.status === 'done'}
							class:running={item.status === 'running'}
							class:error={item.status === 'error'}
							class:expanded={!isCollapsed(item, i)}
						>
							<button class="tool-header" onclick={() => toggleCollapse(item, i)} aria-expanded={!isCollapsed(item, i)}>
								<span class="tool-pip"></span>
								{#if item.status === 'running'}
									<span class="tool-spinner"></span>
								{/if}
								<span class="tool-name">{item.name}</span>
								<span class="tool-status-badge">{item.status}</span>
								{#if isCollapsed(item, i) && toolPreview(item.args)}
									<span class="tool-preview">{toolPreview(item.args)}</span>
								{/if}
								<svg class="tool-chevron" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
									<polyline points="4,2 8,6 4,10" />
								</svg>
							</button>
							{#if !isCollapsed(item, i)}
								<div class="tool-detail">
									{#if item.args && item.args !== '{}'}
										<div class="tool-detail-section">
											<div class="tool-detail-label">params</div>
											<!-- eslint-disable-next-line svelte/no-at-html-tags -->
											<div class="tool-params">{@html renderArgs(item.args)}</div>
										</div>
									{/if}
									{#if item.result}
										<div class="tool-detail-section">
											<div class="tool-detail-label">result</div>
											<div class="tool-result">{item.result}</div>
										</div>
									{:else if item.status === 'running'}
										<div class="running-placeholder">
											<span class="tool-spinner"></span>
											正在执行…
										</div>
									{/if}
								</div>
							{/if}
						</div>
					</div>

				{:else if item.kind === 'error'}
					<div class="item item-error">{item.message}</div>

				{:else if item.kind === 'notice'}
					<div class="item item-notice">{item.message}</div>
				{/if}
			{/each}

			{#if convo.items.length === 0}
				<p class="empty">{isDraft ? '输入消息，开始一段新对话' : '发送消息开始对话'}</p>
			{/if}
		</div>
	</div>

	<!-- INPUT AREA -->
	<div class="input-area">
		<div class="input-inner">
			<div class="input-box">
				<textarea
					class="input-field"
					bind:value={input}
					onkeydown={onKeydown}
					placeholder="输入消息… Enter 发送，Shift+Enter 换行"
					rows="2"
				></textarea>
				<div class="input-actions">
					<span class="input-status">
						{#if incomplete}<span class="status-warn">Turn incomplete</span>{/if}
					</span>
					{#if !isDraft}
						<button class="input-btn cancel" onclick={cancel}>
							<svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">
								<line x1="1" y1="1" x2="9" y2="9" />
								<line x1="9" y1="1" x2="1" y2="9" />
							</svg>
							Cancel
						</button>
					{/if}
					<button class="input-btn primary" disabled={sending} onclick={send}>
						{sending ? 'Sending…' : 'Send'}
						<kbd>↵</kbd>
					</button>
				</div>
			</div>
			<div class="input-hint">Type / for commands</div>
		</div>
	</div>
</div>

<style>
	.conv-page {
		display: flex;
		flex-direction: column;
		height: 100%;
		overflow: hidden;
	}

	/* ---- TOPBAR ---- */
	.topbar {
		height: 44px;
		min-height: 44px;
		border-bottom: 1px solid var(--border-subtle);
		display: flex;
		align-items: center;
		padding: 0 var(--space-6);
		gap: var(--space-3);
		background: var(--canvas-raised);
		flex-shrink: 0;
	}

	.topbar-back {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		border-radius: var(--radius-sm);
		color: var(--text-tertiary);
		flex-shrink: 0;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.topbar-back:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	.topbar-title {
		font-size: 13px;
		font-weight: 500;
		color: var(--text-primary);
		letter-spacing: -0.01em;
		font-family: var(--font-chinese);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 320px;
	}

	.topbar-sep {
		width: 1px;
		height: 14px;
		background: var(--border-default);
		flex-shrink: 0;
	}

	.topbar-meta {
		font-size: 11.5px;
		color: var(--text-tertiary);
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.mono {
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
	}

	.draft-hint {
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.topbar-badge {
		padding: 2px 6px;
		border-radius: 3px;
		font-size: 10.5px;
		font-weight: 510;
		letter-spacing: 0.03em;
	}

	.badge-running {
		background: var(--state-running-bg);
		color: var(--state-running-text);
		border: 1px solid color-mix(in srgb, var(--state-running) 25%, transparent);
	}

	.topbar-actions {
		margin-left: auto;
		display: flex;
		gap: var(--space-1);
	}

	.topbar-btn {
		padding: 4px 10px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border-default);
		background: transparent;
		color: var(--text-secondary);
		font-size: 11.5px;
		font-weight: 450;
		cursor: pointer;
		transition: all var(--dur-fast) var(--ease-out);
	}

	.topbar-btn:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.error-bar {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		padding: var(--space-2) var(--space-6);
		border-bottom: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
		font-size: 12.5px;
		flex-shrink: 0;
	}

	/* ---- CONVERSATION ---- */
	.conv-scroll {
		flex: 1;
		overflow-y: auto;
		padding: var(--space-5) var(--space-10) var(--space-6);
		min-height: 0;
	}

	.conv-inner {
		max-width: 740px;
		margin: 0 auto;
	}

	.item {
		margin-bottom: var(--space-4);
	}

	.empty {
		color: var(--text-tertiary);
		text-align: center;
		margin-top: 25vh;
		font-size: 13px;
		font-family: var(--font-chinese);
	}

	/* ---- USER ---- */
	.item-user {
		display: flex;
		justify-content: flex-end;
	}

	.user-bubble {
		max-width: 560px;
		background: var(--user-bg);
		border: 1px solid var(--user-border);
		border-radius: var(--radius-lg);
		padding: var(--space-3) var(--space-4);
		font-size: 13px;
		color: var(--text-primary);
		line-height: 1.6;
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}

	/* ---- AGENT TEXT ---- */
	.item-text {
		font-size: 13.5px;
		line-height: 1.75;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}

	.item-text :global(p) {
		margin-bottom: var(--space-3);
	}
	.item-text :global(p:last-child) {
		margin-bottom: 0;
	}
	.item-text :global(h1),
	.item-text :global(h2),
	.item-text :global(h3) {
		font-weight: 600;
		margin: 1em 0 0.5em;
		color: var(--text-primary);
		line-height: 1.3;
	}
	.item-text :global(strong) {
		font-weight: 600;
		color: var(--text-primary);
	}
	.item-text :global(code) {
		font-family: var(--font-mono);
		font-size: 12px;
		background: var(--canvas-float);
		color: var(--syntax-str);
		padding: 1px 5px;
		border-radius: 3px;
		border: 1px solid var(--border-subtle);
	}
	.item-text :global(pre) {
		background: var(--canvas-float);
		padding: var(--space-3);
		border-radius: var(--radius-md);
		border: 1px solid var(--border-subtle);
		overflow-x: auto;
		margin: var(--space-3) 0;
	}
	.item-text :global(pre code) {
		background: none;
		border: none;
		padding: 0;
		color: var(--text-secondary);
	}
	.item-text :global(ol),
	.item-text :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-3);
	}
	.item-text :global(li) {
		margin-bottom: var(--space-1);
		padding-left: var(--space-1);
	}
	.item-text :global(li::marker) {
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
	}
	.item-text :global(a) {
		color: var(--accent-ink);
		text-decoration: none;
		border-bottom: 1px solid color-mix(in srgb, var(--accent) 30%, transparent);
		transition: border-color var(--dur-fast);
	}
	.item-text :global(a:hover) {
		border-color: var(--accent);
	}
	.item-text :global(blockquote) {
		border-left: 2px solid var(--border-strong);
		padding-left: var(--space-3);
		color: var(--text-secondary);
		margin: var(--space-3) 0;
	}

	/* Streaming cursor on the live text item */
	.item-text.streaming::after {
		content: '';
		display: inline-block;
		width: 2px;
		height: 1em;
		background: var(--accent);
		vertical-align: text-bottom;
		margin-left: 2px;
		border-radius: 1px;
		animation: cursor-blink 1.1s step-end infinite;
	}

	@keyframes cursor-blink {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0;
		}
	}

	/* ---- REASONING ---- */
	.reasoning-toggle {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-md);
		border: 1px solid var(--reasoning-border);
		background: var(--reasoning-bg);
		cursor: pointer;
		transition:
			border-color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
		width: 100%;
		text-align: left;
	}

	.reasoning-toggle:hover {
		border-color: color-mix(in srgb, var(--reasoning-text) 40%, transparent);
	}

	.reasoning-toggle-icon {
		width: 14px;
		height: 14px;
		flex-shrink: 0;
		color: var(--reasoning-text);
		transition: transform var(--dur-std) var(--ease-out);
	}

	.item-reasoning.expanded .reasoning-toggle-icon {
		transform: rotate(90deg);
	}

	.reasoning-label {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--reasoning-text);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		flex-shrink: 0;
	}

	.reasoning-preview {
		font-size: 12px;
		color: var(--text-tertiary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		flex: 1;
		font-family: var(--font-chinese);
	}

	.reasoning-body {
		padding: var(--space-3) var(--space-3) var(--space-2);
		margin-top: 2px;
		margin-left: var(--space-3);
		border-left: 2px solid var(--reasoning-border);
		font-size: 12.5px;
		color: var(--text-tertiary);
		line-height: 1.7;
		font-family: var(--font-chinese);
		text-wrap: pretty;
	}
	.reasoning-body :global(p) {
		margin-bottom: var(--space-2);
	}
	.reasoning-body :global(p:last-child) {
		margin-bottom: 0;
	}
	.reasoning-body :global(ol),
	.reasoning-body :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-2);
	}
	.reasoning-body :global(li) {
		margin-bottom: 2px;
	}
	.reasoning-body :global(li::marker) {
		color: var(--text-disabled);
		font-variant-numeric: tabular-nums;
	}
	.reasoning-body :global(code) {
		font-family: var(--font-mono);
		font-size: 11.5px;
		background: var(--canvas-float);
		padding: 1px 4px;
		border-radius: 3px;
	}

	/* ---- TOOL (the 120% detail) ---- */
	.tool-block {
		border-radius: var(--radius-md);
		overflow: hidden;
		border: 1px solid var(--border-subtle);
		transition: border-color var(--dur-std) var(--ease-out);
	}

	.tool-block.done {
		border-color: color-mix(in srgb, var(--state-done) 22%, transparent);
	}

	.tool-block.running {
		border-color: color-mix(in srgb, var(--state-running) 35%, transparent);
		animation: tool-running-pulse 2s ease-in-out infinite;
	}

	@keyframes tool-running-pulse {
		0%,
		100% {
			border-color: color-mix(in srgb, var(--state-running) 30%, transparent);
			box-shadow: 0 0 0 0 transparent;
		}
		50% {
			border-color: color-mix(in srgb, var(--state-running) 55%, transparent);
			box-shadow: 0 0 0 3px color-mix(in srgb, var(--state-running) 6%, transparent);
		}
	}

	.tool-block.error {
		border-color: color-mix(in srgb, var(--state-error) 30%, transparent);
	}

	.tool-header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 7px var(--space-3);
		background: var(--canvas-overlay);
		cursor: pointer;
		user-select: none;
		transition: background var(--dur-fast) var(--ease-out);
		width: 100%;
		text-align: left;
	}

	.tool-header:hover {
		background: var(--canvas-float);
	}

	.tool-pip {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		flex-shrink: 0;
		position: relative;
		background: var(--text-tertiary);
	}
	.done .tool-pip {
		background: var(--state-done);
	}
	.running .tool-pip {
		background: var(--state-running);
	}
	.error .tool-pip {
		background: var(--state-error);
	}

	.running .tool-pip::after {
		content: '';
		position: absolute;
		inset: -3px;
		border-radius: 50%;
		border: 1.5px solid var(--state-running);
		opacity: 0;
		animation: pip-ripple 1.8s ease-out infinite;
	}

	@keyframes pip-ripple {
		0% {
			opacity: 0.7;
			transform: scale(0.5);
		}
		100% {
			opacity: 0;
			transform: scale(2.2);
		}
	}

	.tool-spinner {
		width: 11px;
		height: 11px;
		border: 1.5px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		border-top-color: var(--state-running);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
		flex-shrink: 0;
	}

	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	.tool-name {
		font-family: var(--font-mono);
		font-size: 12px;
		font-weight: 500;
		color: var(--text-primary);
		letter-spacing: -0.01em;
		flex-shrink: 0;
	}

	.tool-status-badge {
		font-size: 10px;
		font-weight: 510;
		padding: 1px 5px;
		border-radius: 3px;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		flex-shrink: 0;
	}
	.done .tool-status-badge {
		background: var(--state-done-bg);
		color: var(--state-done-text);
		border: 1px solid color-mix(in srgb, var(--state-done) 25%, transparent);
	}
	.running .tool-status-badge {
		background: var(--state-running-bg);
		color: var(--state-running-text);
		border: 1px solid color-mix(in srgb, var(--state-running) 25%, transparent);
	}
	.error .tool-status-badge {
		background: var(--state-error-bg);
		color: var(--state-error-text);
		border: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
	}

	.tool-preview {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-tertiary);
		font-family: var(--font-mono);
		font-size: 11px;
	}

	.tool-chevron {
		margin-left: auto;
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-std) var(--ease-out);
		flex-shrink: 0;
	}

	.tool-block.expanded .tool-chevron {
		transform: rotate(90deg);
	}

	.tool-detail {
		background: var(--canvas-base);
		border-top: 1px solid var(--border-subtle);
	}

	.tool-detail-section {
		padding: var(--space-3) var(--space-4);
		border-bottom: 1px solid var(--border-subtle);
	}
	.tool-detail-section:last-child {
		border-bottom: none;
	}

	.tool-detail-label {
		font-size: 10px;
		font-weight: 510;
		color: var(--text-tertiary);
		text-transform: uppercase;
		letter-spacing: 0.1em;
		margin-bottom: var(--space-2);
	}

	.tool-params {
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre;
		overflow-x: auto;
		max-height: 200px;
	}

	.tool-params :global(.syn-key) {
		color: var(--syntax-key);
	}
	.tool-params :global(.syn-str) {
		color: var(--syntax-str);
	}
	.tool-params :global(.syn-num) {
		color: var(--syntax-num);
	}

	.tool-result {
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre-wrap;
		word-break: break-word;
		max-height: 220px;
		overflow-y: auto;
	}

	.running-placeholder {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-size: 12px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		padding: var(--space-3) var(--space-4);
	}

	/* ---- ERROR / NOTICE ---- */
	.item-error {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		border: 1px solid color-mix(in srgb, var(--state-error) 30%, transparent);
		border-radius: var(--radius-md);
		padding: var(--space-3) var(--space-4);
		font-size: 12.5px;
	}

	.item-notice {
		color: var(--text-tertiary);
		font-size: 12px;
		font-style: italic;
		text-align: center;
		padding: var(--space-2) var(--space-4);
		font-family: var(--font-chinese);
	}

	/* Streaming dot (reasoning / tool live indicator) */
	.streaming-dot {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--accent);
		animation: pulse 1s ease-in-out infinite;
		flex-shrink: 0;
	}

	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.3;
		}
	}

	/* ---- INPUT AREA ---- */
	.input-area {
		border-top: 1px solid var(--border-subtle);
		padding: var(--space-4) var(--space-10);
		background: var(--canvas-raised);
		flex-shrink: 0;
	}

	.input-inner {
		max-width: 740px;
		margin: 0 auto;
	}

	.input-box {
		background: var(--canvas-overlay);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-lg);
		overflow: hidden;
		transition:
			border-color var(--dur-std) var(--ease-out),
			box-shadow var(--dur-std) var(--ease-out);
	}

	.input-box:focus-within {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 8%, transparent);
	}

	.input-field {
		width: 100%;
		padding: var(--space-3) var(--space-4);
		background: transparent;
		border: none;
		outline: none;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		font-size: 13px;
		line-height: 1.6;
		resize: none;
		min-height: 44px;
		max-height: 120px;
	}

	.input-field::placeholder {
		color: var(--text-disabled);
	}

	.input-actions {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-top: 1px solid var(--border-subtle);
	}

	.input-status {
		font-size: 11px;
		color: var(--text-tertiary);
		flex: 1;
		font-family: var(--font-chinese);
	}

	.input-status .status-warn {
		color: var(--state-running-text);
	}

	.input-btn {
		display: flex;
		align-items: center;
		gap: 5px;
		padding: 5px 10px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border-default);
		background: transparent;
		color: var(--text-secondary);
		font-size: 11.5px;
		font-weight: 450;
		cursor: pointer;
		font-family: var(--font-sans);
		transition: all var(--dur-fast) var(--ease-out);
	}

	.input-btn:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.input-btn.primary {
		background: var(--accent);
		color: var(--accent-fg);
		border-color: transparent;
		font-weight: 590;
	}

	.input-btn.primary:hover {
		background: var(--accent-hover);
		color: var(--accent-fg);
	}

	.input-btn.primary:disabled {
		opacity: 0.55;
		cursor: not-allowed;
	}

	.input-btn.cancel {
		color: var(--state-error-text);
		border-color: color-mix(in srgb, var(--state-error) 22%, transparent);
	}

	.input-btn.cancel:hover {
		background: var(--state-error-bg);
		border-color: color-mix(in srgb, var(--state-error) 40%, transparent);
		color: var(--state-error-text);
	}

	kbd {
		font-family: var(--font-mono);
		font-size: 10px;
		background: var(--canvas-float);
		border: 1px solid var(--border-strong);
		border-radius: 3px;
		padding: 1px 4px;
		color: var(--text-tertiary);
	}

	.input-hint {
		font-size: 10.5px;
		color: var(--text-disabled);
		font-family: var(--font-mono);
		margin-top: 5px;
		padding-left: 2px;
	}

	@media (prefers-reduced-motion: reduce) {
		.tool-block.running {
			animation: none;
		}
		.tool-spinner {
			animation: none;
		}
		.running .tool-pip::after {
			animation: none;
		}
		.item-text.streaming::after {
			animation: none;
		}
		.streaming-dot {
			animation: none;
		}
	}
</style>
