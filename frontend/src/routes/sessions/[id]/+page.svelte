<script lang="ts">
	import { onDestroy } from 'svelte';
	import { browser } from '$app/environment';
	import { page } from '$app/state';
	import { marked } from 'marked';
	import DOMPurify from 'dompurify';
	import { client } from '$lib/client';
	import type { EventSubscription } from '$lib/client-core';
	import { apply, emptyState, type ConversationState, type Item } from '$lib/conversation';
	import Button from '$lib/components/Button.svelte';

	let convo = $state<ConversationState>(emptyState());
	let input = $state('');
	let sending = $state(false);
	let error = $state<string | null>(null);
	let sessionId = $state(page.params.id!);
	let sub: EventSubscription | undefined;
	let streamEl = $state<HTMLElement | null>(null);
	// Track collapsed state for reasoning items separately (index → collapsed)
	let collapsed = $state<Record<number, boolean>>({});

	function subscribe(id: string) {
		sub?.close();
		convo = emptyState();
		collapsed = {};
		sub = client.subscribeEvents(id, {
			onEvent: (ev) => {
				convo = apply(convo, ev);
				if (ev.type === 'compacted') {
					sessionId = ev.new_session_id;
					subscribe(ev.new_session_id);
				}
				// Auto-scroll on new content
				requestAnimationFrame(() => {
					streamEl?.scrollTo({ top: streamEl.scrollHeight, behavior: 'smooth' });
				});
			},
			onError: (e) => {
				error = e instanceof Error ? e.message : String(e);
			}
		});
	}

	$effect(() => {
		const id = sessionId;
		subscribe(id);
	});

	onDestroy(() => sub?.close());

	async function send() {
		const text = input.trim();
		if (!text || sending) return;
		sending = true;
		error = null;
		try {
			await client.sendMessage(sessionId, text);
			input = '';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			sending = false;
		}
	}

	async function cancel() {
		try { await client.cancel(sessionId); }
		catch (e) { error = e instanceof Error ? e.message : String(e); }
	}

	async function compact() {
		try { await client.compact(sessionId); }
		catch (e) { error = e instanceof Error ? e.message : String(e); }
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
</script>

<div class="page">
	<header>
		<a href="/sessions" class="back">← 返回</a>
		<span class="sid">{sessionId}</span>
		<div class="actions">
			<Button variant="ghost" onclick={cancel}>中断</Button>
			<Button variant="ghost" onclick={compact}>压缩</Button>
		</div>
	</header>

	{#if error}
		<div class="error-bar">{error}</div>
	{/if}

	<div class="stream" bind:this={streamEl}>
		{#each convo.items as item, i (i)}
			{#if item.kind === 'user'}
				<div class="msg user">{item.text}</div>

			{:else if item.kind === 'text'}
				{#if item.text.trim()}
					<div class="msg text" class:streaming={item.streaming}>
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
					<div class="msg reasoning" class:collapsed={isCollapsed(item, i)}>
						<button class="reasoning-header" onclick={() => toggleCollapse(item, i)}>
							<span class="reasoning-icon">{isCollapsed(item, i) ? '▸' : '▾'}</span>
							<span class="reasoning-label">思考过程</span>
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
				<div class="msg tool" class:done={item.status === 'done'} class:err={item.status === 'error'}>
					<button class="tool-header" onclick={() => toggleCollapse(item, i)}>
						<span class="tool-icon">
							{#if item.status === 'running'}⋯{:else if item.status === 'done'}✓{:else}✗{/if}
						</span>
						<span class="tool-name">{item.name}</span>
						{#if isCollapsed(item, i) && toolPreview(item.args)}
							<span class="tool-preview">{toolPreview(item.args)}</span>
						{/if}
						{#if item.status === 'running'}
							<span class="streaming-dot"></span>
						{:else}
							<span class="tool-toggle">{isCollapsed(item, i) ? '▸' : '▾'}</span>
						{/if}
					</button>
					{#if !isCollapsed(item, i)}
						{#if item.args && item.args !== '{}'}
							<pre class="tool-args">{item.args}</pre>
						{/if}
						{#if item.result}
							<div class="tool-result">{item.result}</div>
						{/if}
					{/if}
				</div>

			{:else if item.kind === 'error'}
				<div class="msg error-item">{item.message}</div>

			{:else if item.kind === 'notice'}
				<div class="msg notice">{item.message}</div>
			{/if}
		{/each}

		{#if convo.items.length === 0}
			<p class="empty">发送消息开始对话</p>
		{/if}
	</div>

	<div class="composer">
		<textarea
			bind:value={input}
			onkeydown={onKeydown}
			placeholder="输入消息… (Enter 发送，Shift+Enter 换行)"
			rows="2"
		></textarea>
		<Button variant="accent" disabled={sending} onclick={send}>
			{sending ? '发送中…' : '发送'}
		</Button>
	</div>
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		height: calc(100vh - var(--gap-2xl) * 2);
		gap: var(--gap-lg);
	}

	header {
		display: flex;
		align-items: center;
		gap: var(--gap-lg);
		padding-bottom: var(--gap-lg);
		border-bottom: 1px solid var(--border);
		flex-shrink: 0;
	}

	.back {
		color: var(--text-secondary);
		font-size: 14px;
		font-weight: 500;
		white-space: nowrap;
	}

	.back:hover { color: var(--accent); }

	.sid {
		font-family: var(--font-mono);
		font-size: 12px;
		color: var(--text-muted);
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		padding: 4px var(--gap-sm);
		background: var(--bg-tertiary);
		border-radius: var(--radius-sm);
	}

	.actions { display: flex; gap: var(--gap-sm); }

	.error-bar {
		color: var(--error);
		background: var(--error-bg);
		padding: var(--gap-sm) var(--gap-md);
		border-radius: var(--radius-md);
		border-left: 3px solid var(--error);
		font-size: 14px;
		flex-shrink: 0;
	}

	.stream {
		flex: 1;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		gap: var(--gap-md);
		/* Custom scrollbar at edge */
		scrollbar-gutter: stable;
	}

	.empty {
		color: var(--text-muted);
		text-align: center;
		margin: auto;
		font-size: 14px;
	}

	.msg {
		padding: var(--gap-md) var(--gap-lg);
		border-radius: var(--radius-md);
		font-size: 15px;
		line-height: 1.7;
		word-break: break-word;
	}

	.msg.user {
		background: var(--accent);
		color: var(--accent-fg);
		align-self: flex-end;
		max-width: 80%;
		border-radius: var(--radius-lg);
	}

	.msg.text {
		background: var(--surface);
		border: 1px solid var(--border);
	}

	/* Markdown content styling */
	.msg.text :global(p) { margin-bottom: 0.75em; }
	.msg.text :global(p:last-child) { margin-bottom: 0; }
	.msg.text :global(h1),
	.msg.text :global(h2),
	.msg.text :global(h3) {
		font-weight: 600;
		margin: 1em 0 0.5em;
		color: var(--text-primary);
	}
	.msg.text :global(code) {
		font-family: var(--font-mono);
		font-size: 13px;
		background: var(--bg-tertiary);
		padding: 2px 5px;
		border-radius: 3px;
	}
	.msg.text :global(pre) {
		background: var(--bg-tertiary);
		padding: var(--gap-md);
		border-radius: var(--radius-md);
		overflow-x: auto;
		margin: 0.75em 0;
	}
	.msg.text :global(pre code) {
		background: none;
		padding: 0;
	}
	.msg.text :global(ul), .msg.text :global(ol) {
		padding-left: 1.5em;
		margin: 0.5em 0;
	}
	.msg.text :global(blockquote) {
		border-left: 3px solid var(--border-hover);
		padding-left: var(--gap-md);
		color: var(--text-secondary);
		margin: 0.75em 0;
	}

	.msg.reasoning {
		background: transparent;
		border: 1px dashed var(--border);
	}

	.reasoning-header {
		display: flex;
		align-items: center;
		gap: var(--gap-sm);
		width: 100%;
		text-align: left;
		font-size: 13px;
		color: var(--text-muted);
		padding: 0;
		cursor: pointer;
	}

	.reasoning-header:hover { color: var(--text-secondary); }

	.reasoning-icon { font-size: 11px; }

	.reasoning-label {
		font-weight: 500;
		color: var(--info);
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}

	.reasoning-preview {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		opacity: 0.7;
	}

	.reasoning-body {
		margin-top: var(--gap-sm);
		font-size: 13px;
		color: var(--text-muted);
		white-space: pre-wrap;
		line-height: 1.6;
	}

	.msg.tool {
		background: var(--bg-secondary);
		border: 1px solid var(--border);
		padding: var(--gap-md);
	}

	.msg.tool.done { border-color: var(--success); }
	.msg.tool.err { border-color: var(--error); }

	.tool-header {
		display: flex;
		align-items: center;
		gap: var(--gap-sm);
		font-size: 13px;
		font-family: var(--font-mono);
		width: 100%;
		text-align: left;
		padding: 0;
		cursor: pointer;
	}

	.tool-header:hover .tool-name { color: var(--accent); }

	.tool-toggle {
		margin-left: auto;
		font-size: 11px;
		color: var(--text-muted);
	}

	.tool-icon {
		font-size: 12px;
		.msg.tool.done & { color: var(--success); }
		.msg.tool.err & { color: var(--error); }
		.msg.tool:not(.done):not(.err) & { color: var(--text-muted); }
	}

	.tool-name {
		font-weight: 600;
		color: var(--text-primary);
		font-size: 13px;
	}

	.tool-preview {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-muted);
		font-size: 12px;
		opacity: 0.8;
	}

	.tool-args {
		margin-top: var(--gap-sm);
		font-family: var(--font-mono);
		font-size: 12px;
		color: var(--text-muted);
		white-space: pre-wrap;
		word-break: break-all;
		max-height: 120px;
		overflow-y: auto;
	}

	.tool-result {
		margin-top: var(--gap-sm);
		padding-top: var(--gap-sm);
		border-top: 1px solid var(--border);
		font-size: 13px;
		color: var(--text-secondary);
		white-space: pre-wrap;
		word-break: break-word;
		max-height: 160px;
		overflow-y: auto;
	}

	.msg.error-item {
		color: var(--error);
		background: var(--error-bg);
		border: 1px solid var(--error);
		font-size: 14px;
	}

	.msg.notice {
		color: var(--text-muted);
		font-size: 13px;
		font-style: italic;
		border: none;
		background: none;
		padding: var(--gap-sm) var(--gap-md);
		text-align: center;
	}

	.streaming::after {
		content: '▋';
		color: var(--accent);
		animation: blink 0.8s step-start infinite;
	}

	.streaming-dot {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--accent);
		animation: pulse 1s ease-in-out infinite;
	}

	@keyframes blink { 50% { opacity: 0; } }
	@keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.3; } }

	.composer {
		display: flex;
		gap: var(--gap-md);
		align-items: flex-end;
		flex-shrink: 0;
		padding-top: var(--gap-md);
		border-top: 1px solid var(--border);
	}

	textarea {
		flex: 1;
		resize: none;
		padding: var(--gap-md) var(--gap-lg);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		background: var(--surface);
		color: var(--text-primary);
		font-family: var(--font-sans);
		font-size: 14px;
		line-height: 1.5;
		transition: border-color var(--motion-fast);
	}

	textarea:focus {
		border-color: var(--accent);
		outline: none;
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 15%, transparent);
	}

	textarea::placeholder { color: var(--text-muted); }
</style>
