<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api, errorMessage } from '#lib/api.ts';
	import { session } from '#lib/session.svelte.ts';

	let username = $state('');
	let password = $state('');
	let error = $state('');
	let busy = $state(false);

	/** Where to go after logging in: a path on this site, or the start page. */
	function next() {
		const target = page.url.searchParams.get('next') ?? '/';
		return target.startsWith('/') && !target.startsWith('//') ? target : '/';
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = '';
		try {
			const s = await api.login(username, password);
			session.username = s.username;
			await goto(next(), { replaceState: true });
		} catch (e) {
			error = errorMessage(e);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>Log in · manage.nix</title></svelte:head>

<div class="center">
	<form class="card stack" onsubmit={submit}>
		<h1>manage.nix</h1>
		<label>
			Username
			<input name="username" bind:value={username} autocomplete="username" required />
		</label>
		<label>
			Password
			<input
				name="password"
				type="password"
				bind:value={password}
				autocomplete="current-password"
				required
			/>
		</label>
		{#if error}<p class="error" role="alert">{error}</p>{/if}
		<button class="primary" type="submit" disabled={busy}>Log in</button>
	</form>
</div>

<style>
	.center {
		min-height: 100vh;
		display: grid;
		place-items: center;
		padding: 1rem;
	}
	form {
		width: min(22rem, 100%);
		padding: 1.75rem;
	}
	h1 {
		margin-bottom: 0.25rem;
	}
</style>
