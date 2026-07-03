<script lang="ts">
	// Session route: `/workspaces/[wsId]/sessions/[id]` shows one existing session
	// in this workspace. Both ids come from the route params; the Conversation
	// component re-subscribes when `routeSessionId` changes (sidebar navigation).
	import { page } from '$app/state';
	import Conversation from '$lib/components/Conversation.svelte';

	const workspaceId = $derived(page.params.wsId!);
	const routeSessionId = $derived(page.params.id!);
</script>

<Conversation {workspaceId} {routeSessionId} />
