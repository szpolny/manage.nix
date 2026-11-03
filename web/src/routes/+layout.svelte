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

	// Icon paths: 24×24 strokes in the style of Lucide.
	const links = [
		{
			href: '/',
			label: 'Machines',
			icon: 'M4 5h16v10H4zM8 19h8M12 15v4'
		},
		{
			href: '/deploys',
			label: 'Deploys',
			// A system delivered down onto a server.
			icon: 'M4 14h16v7H4zM7.5 17.5h.01M12 3v8M8.5 7.5 12 11l3.5-3.5'
		},
		{
			href: '/repositories',
			label: 'Repositories',
			icon: 'M6 3v12M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM18 9a9 9 0 0 1-9 9'
		}
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
	<div class="shell">
		<aside class="sidebar">
			<a class="brand" href="/">manage<span>.nix</span></a>
			<nav aria-label="Main">
				{#each links as link (link.href)}
					<a href={link.href} aria-current={active(link.href) ? 'page' : undefined}>
						<svg viewBox="0 0 24 24" aria-hidden="true"><path d={link.icon} /></svg>
						{link.label}
					</a>
				{/each}
			</nav>
			{#if session.username}
				<div class="user">
					<a
						class="account"
						href="/account"
						aria-current={active('/account') ? 'page' : undefined}
					>
						<span class="name">{session.username}</span>
					</a>
					<button type="button" class="logout" onclick={logout} aria-label="Log out" title="Log out">
						<svg viewBox="0 0 24 24" aria-hidden="true"
							><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9" /></svg
						>
					</button>
				</div>
			{/if}
		</aside>
		<main>
			{#if checked && session.username}
				{@render children()}
			{/if}
		</main>
	</div>
{/if}

<style>
	.shell {
		display: grid;
		grid-template-columns: 13.5rem minmax(0, 1fr);
		min-height: 100vh;
	}
	.sidebar {
		position: sticky;
		top: 0;
		height: 100vh;
		display: flex;
		flex-direction: column;
		gap: 1.75rem;
		padding: 1.1rem 0.75rem;
		border-right: 1px solid var(--border);
	}
	.brand {
		padding: 0.2rem 0.6rem;
		color: var(--text);
		font-family: var(--mono);
		font-size: 0.95rem;
		font-weight: 500;
	}
	.brand span {
		color: var(--muted);
	}
	.brand:hover {
		text-decoration: none;
	}
	nav {
		display: grid;
		gap: 1px;
	}
	nav a,
	.account {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		padding: 0.4rem 0.6rem;
		border-radius: var(--radius-sm);
		color: var(--muted);
		font-weight: 500;
	}
	nav a:hover,
	.account:hover {
		background: var(--hover);
		color: var(--text);
		text-decoration: none;
	}
	nav a[aria-current='page'],
	.account[aria-current='page'] {
		background: var(--surface);
		color: var(--text);
		box-shadow: inset 0 0 0 1px var(--border);
	}
	nav a[aria-current='page'] svg {
		color: var(--text);
	}
	svg {
		width: 1.05rem;
		height: 1.05rem;
		flex: none;
		fill: none;
		stroke: currentColor;
		stroke-width: 1.7;
		stroke-linecap: round;
		stroke-linejoin: round;
		color: var(--muted);
	}
	.user {
		margin-top: auto;
		display: flex;
		align-items: center;
		gap: 0.25rem;
		padding-top: 0.75rem;
		border-top: 1px solid var(--border);
	}
	.account {
		flex: 1;
		min-width: 0;
		font-family: var(--mono);
		font-size: 0.85rem;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.logout {
		border: none;
		background: none;
		padding: 0.4rem;
	}
	.logout:hover:not(:disabled) {
		background: var(--hover);
	}
	.logout:hover svg {
		color: var(--text);
	}
	main {
		width: 100%;
		max-width: 80rem;
		margin: 0 auto;
		padding: 2rem 2.25rem 3rem;
	}
	@media (max-width: 52rem) {
		.shell {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: auto 1fr;
		}
		.sidebar {
			align-content: start;
			z-index: 1;
			height: auto;
			flex-direction: row;
			flex-wrap: wrap;
			align-items: center;
			gap: 0.5rem 1rem;
			padding: 0.6rem 0.75rem;
			background: var(--bg);
			border-right: none;
			border-bottom: 1px solid var(--border);
		}
		nav {
			display: flex;
			order: 3;
			width: 100%;
			overflow-x: auto;
		}
		.user {
			margin: 0 0 0 auto;
			padding: 0;
			border: none;
		}
		main {
			padding: 1.25rem 0.9rem 2rem;
		}
	}
</style>
