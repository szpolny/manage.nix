<script lang="ts">
	import { api, errorMessage } from '#lib/api.ts';
	import { session } from '#lib/session.svelte.ts';

	let current = $state('');
	let next = $state('');
	let repeat = $state('');
	let error = $state('');
	let done = $state(false);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		error = '';
		done = false;
		if (next !== repeat) {
			error = 'The new passwords do not match.';
			return;
		}
		try {
			await api.changePassword(current, next);
			current = next = repeat = '';
			done = true;
		} catch (e) {
			error = errorMessage(e);
		}
	}
</script>

<svelte:head><title>Account · manage.nix</title></svelte:head>

<div class="page-header"><h1>{session.username}</h1></div>

<form class="card stack narrow" onsubmit={submit}>
	<h2>Change password</h2>
	<label>
		Current password
		<input type="password" bind:value={current} autocomplete="current-password" required />
	</label>
	<label>
		New password
		<input type="password" bind:value={next} autocomplete="new-password" minlength="8" required />
	</label>
	<label>
		Repeat the new password
		<input type="password" bind:value={repeat} autocomplete="new-password" required />
	</label>
	{#if error}<p class="error" role="alert">{error}</p>{/if}
	{#if done}<p role="status">Password changed. Your other sessions have been logged out.</p>{/if}
	<div class="actions"><button class="primary" type="submit">Change password</button></div>
</form>

<style>
	.narrow {
		max-width: 26rem;
	}
</style>
