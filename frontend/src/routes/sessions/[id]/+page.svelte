<script lang="ts">
	import { onDestroy } from 'svelte';
	import { page } from '$app/state';
	import { client } from '$lib/client';
	import type { EventSubscription } from '$lib/client-core';
	import { apply, emptyState, type ConversationState } from '$lib/conversation';
	import Button from '$lib/components/Button.svelte';

	let convo = $state<ConversationState>(emptyState());
	let input = $state('');
	let sending = $state(false);
	let error = $state<string | null>(null);

	// The session being viewed. Tracked as state because a compaction switches
	// us to a new session id (we resubscribe to follow it, gateway.md §2.1).
	// The [id] route param is always present here.
	let sessionId = $state(page.params.id!);
	let sub: EventSubscription | undefined;

	function subscribe(id: string) {
		sub?.close();
		convo = emptyState();
		sub = client.subscribeEvents(id, {
			onEvent: (ev) => {
				convo = apply(convo, ev);
				// Follow a compaction to the new session.
				if (ev.type === 'compacted') {
					sessionId = ev.new_session_id;
					subscribe(ev.new_session_id);
				}
			},
			onError: (e) => {
				error = e instanceof Error ? e.message : String(e);
			}
		});
	}

	// (Re)subscribe whenever the viewed session id changes.
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
		try {
			await client.cancel(sessionId);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

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
</script>

<header>
	<a href="/sessions" class="back">← Sessions</a>
	<span class="sid">{sessionId}</span>
	<div class="actions">
		<Button variant="ghost" onclick={cancel}>Cancel</Button>
		<Button variant="ghost" onclick={compact}>Compact</Button>
	</div>
</header>

{#if error}
	<p class="error">{error}</p>
{/if}

<div class="stream">
	{#each convo.items as item, i (i)}
		{#if item.kind === 'user'}
			<div class="msg user">{item.text}</div>
		{:else if item.kind === 'text'}
			<div class="msg text" class:streaming={item.streaming}>{item.text}</div>
		{:else if item.kind === 'reasoning'}
			<div class="msg reasoning" class:streaming={item.streaming}>{item.text}</div>
		{:else if item.kind === 'tool_call'}
			<div class="msg tool-call">
				<span class="label">tool · {item.name}</span>
				<pre>{item.args}</pre>
			</div>
		{:else if item.kind === 'tool_result'}
			<div class="msg tool-result" class:err={!item.ok}>
				<span class="label">result{item.ok ? '' : ' · error'}</span>
				<pre>{item.text}</pre>
			</div>
		{:else if item.kind === 'error'}
			<div class="msg error-item">{item.message}</div>
		{:else if item.kind === 'notice'}
			<div class="msg notice">{item.message}</div>
		{/if}
	{/each}
	{#if convo.items.length === 0}
		<p class="muted">No messages yet. Send something to start the turn.</p>
	{/if}
</div>

<div class="composer">
	<textarea
		bind:value={input}
		onkeydown={onKeydown}
		placeholder="Message… (Enter to send, Shift+Enter for newline)"
		rows="2"
	></textarea>
	<Button variant="accent" disabled={sending} onclick={send}>
		{sending ? 'Sending…' : 'Send'}
	</Button>
</div>

<style>
	header {
		display: flex;
		align-items: center;
		gap: var(--gap-lg);
		margin-bottom: var(--gap-lg);
	}

	.back {
		color: var(--text-secondary);
		font-size: 13px;
	}

	.sid {
		font-family: var(--font-mono);
		font-size: 12px;
		color: var(--text-muted);
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.actions {
		display: flex;
		gap: var(--gap-xs);
	}

	.error {
		color: var(--error);
		background: var(--error-bg);
		padding: var(--gap-sm) var(--gap-md);
		border-radius: var(--radius-md);
		margin-bottom: var(--gap-md);
		font-size: 13px;
	}

	.stream {
		display: flex;
		flex-direction: column;
		gap: var(--gap-md);
		margin-bottom: var(--gap-xl);
	}

	.msg {
		padding: var(--gap-md);
		border-radius: var(--radius-md);
		font-size: 14px;
		white-space: pre-wrap;
		word-break: break-word;
	}

	.msg.user {
		background: var(--accent-weak);
		align-self: flex-end;
		max-width: 80%;
	}

	.msg.text {
		background: var(--surface);
	}

	.msg.reasoning {
		background: transparent;
		border-left: 2px solid var(--border);
		color: var(--text-muted);
		font-size: 13px;
		padding-left: var(--gap-md);
	}

	.msg.tool-call,
	.msg.tool-result {
		background: var(--bg-secondary);
		border: 1px solid var(--border);
	}

	.msg.tool-result.err {
		border-color: var(--error);
	}

	.label {
		display: block;
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-muted);
		margin-bottom: var(--gap-xs);
	}

	.msg pre {
		font-family: var(--font-mono);
		font-size: 12px;
		white-space: pre-wrap;
		word-break: break-word;
	}

	.msg.error-item {
		color: var(--error);
		background: var(--error-bg);
		font-size: 13px;
	}

	.msg.notice {
		color: var(--text-muted);
		font-size: 12px;
		font-style: italic;
	}

	.streaming::after {
		content: '▋';
		color: var(--accent);
		animation: blink 1s step-start infinite;
	}

	@keyframes blink {
		50% {
			opacity: 0;
		}
	}

	.muted {
		color: var(--text-muted);
	}

	.composer {
		display: flex;
		gap: var(--gap-sm);
		align-items: flex-end;
		position: sticky;
		bottom: 0;
		background: var(--bg-primary);
		padding-top: var(--gap-sm);
	}

	textarea {
		flex: 1;
		resize: vertical;
		padding: var(--gap-sm) var(--gap-md);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		background: var(--surface);
		color: var(--text-primary);
		font-family: var(--font-sans);
		font-size: 14px;
	}
</style>
