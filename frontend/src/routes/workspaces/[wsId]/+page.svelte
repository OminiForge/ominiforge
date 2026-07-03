<script lang="ts">
	// Draft route: `/workspaces/[wsId]` shows a new (not-yet-created) session in
	// this workspace. The Conversation component handles the draft lifecycle
	// (empty view → create on first send → adopt the real id + URL).
	import { page } from '$app/state';
	import Conversation from '$lib/components/Conversation.svelte';

	const workspaceId = $derived(page.params.wsId!);
</script>

<Conversation {workspaceId} routeSessionId="new" />
