<script lang="ts">
	import { onMount } from 'svelte';
	import { api, errorMessage } from '#lib/api.ts';

	let info = $state<{ version: string; revision: string | null } | null>(null);
	let error = $state('');

	onMount(() => {
		api.version().then((value) => (info = value)).catch((e) => (error = errorMessage(e)));
	});
</script>

<svelte:head><title>About · manage.nix</title></svelte:head>

<h1>About manage.nix</h1>
<p>Version information for the running server.</p>
{#if error}
	<p role="alert">Could not load version information: {error}</p>
{:else if info}
	<dl>
		<dt>Version</dt>
		<dd>{info.version}</dd>
		<dt>Build revision</dt>
		<dd>{info.revision ?? 'Not available'}</dd>
	</dl>
{:else}
	<p role="status">Loading version information…</p>
{/if}

<style>
	dl { display: grid; gap: 0.5rem; }
	dt { color: var(--muted); }
	dd { margin: 0 0 1rem; font-family: var(--mono); overflow-wrap: anywhere; }
</style>
