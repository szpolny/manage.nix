<script lang="ts">
	import '../app.css';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api } from '#lib/api.ts';
	import { session } from '#lib/session.svelte.ts';

	let { children } = $props();

	let onLogin = $derived(page.url.pathname === '/login');
	let checked = $state(false);

	$effect(() => {
		if (onLogin || session.username) {
			checked = true;
			return;
		}
		// A 401 sends the API client to the login page.
		api
			.session()
			.then((s) => (session.username = s.username))
			.catch(() => {})
			.finally(() => (checked = true));
	});

	const links = [
		{ href: '/', label: 'Machines' },
		{ href: '/deploys', label: 'Deploys' },
		{ href: '/repositories', label: 'Repositories' }
	];

	function active(href: string) {
		const path = page.url.pathname;
		return href === '/' ? path === '/' || path.startsWith('/machines') : path.startsWith(href);
	}

	async function logout() {
		await api.logout().catch(() => {});
		session.username = null;
		await goto('/login');
	}
</script>

{#if onLogin}
	{@render children()}
{:else}
	<header class="top">
		<nav>
			<a class="brand" href="/">manage.nix</a>
			{#each links as link (link.href)}
				<a href={link.href} aria-current={active(link.href) ? 'page' : undefined}>{link.label}</a>
			{/each}
		</nav>
		{#if session.username}
			<div class="user">
				<a href="/account" aria-current={active('/account') ? 'page' : undefined}
					>{session.username}</a
				>
				<button type="button" onclick={logout}>Log out</button>
			</div>
		{/if}
	</header>
	<main>
		{#if checked && session.username}
			{@render children()}
		{/if}
	</main>
{/if}

<style>
	.top {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 1rem;
		padding: 0.6rem 1.5rem;
		background: var(--surface);
		border-bottom: 1px solid var(--border);
		position: sticky;
		top: 0;
		z-index: 1;
	}
	nav,
	.user {
		display: flex;
		align-items: center;
		gap: 1.25rem;
	}
	nav a,
	.user a {
		color: var(--muted);
		font-weight: 500;
	}
	nav a[aria-current='page'],
	.user a[aria-current='page'] {
		color: var(--text);
	}
	.brand {
		color: var(--text) !important;
		font-weight: 700;
		margin-right: 0.5rem;
	}
	main {
		max-width: 76rem;
		margin: 0 auto;
		padding: 1.5rem;
	}
</style>
