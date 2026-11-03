<script lang="ts">
	import { onMount } from 'svelte';
	import { api, errorMessage } from '#lib/api.ts';
	import Badge from '#lib/Badge.svelte';
	import DeployDialog from '#lib/DeployDialog.svelte';
	import Dialog from '#lib/Dialog.svelte';
	import EnrollmentInstructions from '#lib/EnrollmentInstructions.svelte';
	import { ago, exact, revisionLabel, shortPath } from '#lib/format.ts';
	import type { CreatedMachine, Machine, UpdateSummary } from '#lib/types.ts';

	let machines: Machine[] = $state([]);
	let loaded = $state(false);
	let error = $state('');
	let selected: string[] = $state([]);
	let now = $state(Date.now());
	let updates: Record<string, UpdateSummary> = $state({});

	let deployDialog: DeployDialog;
	let addDialog: Dialog;
	let newName = $state('');
	let addError = $state('');
	let created: CreatedMachine | null = $state(null);

	async function refresh() {
		try {
			machines = await api.machines();
			now = Date.now();
			error = '';
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loaded = true;
		}
	}

	// Update checks fetch repositories, so they run less often.
	async function refreshUpdates() {
		try {
			const list = await api.allUpdates();
			updates = Object.fromEntries(list.map((u) => [u.machine, u]));
		} catch {
			// The machines are still worth showing.
		}
	}

	onMount(() => {
		refresh();
		refreshUpdates();
		const timer = setInterval(refresh, 5000);
		const updatesTimer = setInterval(refreshUpdates, 60_000);
		return () => {
			clearInterval(timer);
			clearInterval(updatesTimer);
		};
	});

	function openAdd() {
		newName = '';
		addError = '';
		created = null;
		addDialog.show();
	}

	async function add(event: SubmitEvent) {
		event.preventDefault();
		addError = '';
		try {
			created = await api.addMachine(newName.trim());
			await refresh();
		} catch (e) {
			addError = errorMessage(e);
		}
	}

	type Filter = 'all' | 'online' | 'offline' | 'pending' | 'attention';
	let filter: Filter = $state('all');
	let query = $state('');

	function needsAttention(m: Machine) {
		return Boolean(
			updates[m.name]?.behind ||
				updates[m.name]?.nixpkgs_available ||
				m.facts?.flatpak_updates?.length ||
				m.facts?.reboot_required
		);
	}

	const matches: Record<Filter, (m: Machine) => boolean> = {
		all: () => true,
		online: (m) => m.status === 'online',
		offline: (m) => m.status === 'offline',
		pending: (m) => m.status === 'pending',
		attention: needsAttention
	};

	let tiles = $derived(
		(
			[
				['all', 'All', ''],
				['online', 'Online', 'ok'],
				['offline', 'Offline', 'bad'],
				['pending', 'Waiting to enroll', 'idle'],
				['attention', 'Need attention', 'warn']
			] as const
		).map(([key, label, tone]) => ({
			key,
			label,
			tone,
			count: machines.filter(matches[key]).length
		}))
	);

	const osNames: Record<string, string> = { nixos: 'NixOS', darwin: 'macOS' };
	function osName(os: string) {
		return osNames[os] ?? os;
	}

	let visible = $derived.by(() => {
		const q = query.trim().toLowerCase();
		return machines.filter(
			(m) =>
				matches[filter](m) &&
				(!q || m.name.toLowerCase().includes(q) || m.tags.some((t) => t.toLowerCase().includes(q)))
		);
	});
</script>

<svelte:head><title>Machines · manage.nix</title></svelte:head>

<div class="page-header">
	<h1>Machines</h1>
	<div class="actions">
		<button type="button" onclick={openAdd}>Add machine</button>
		<button
			type="button"
			class="primary"
			onclick={() => deployDialog.open(selected)}
			disabled={!machines.some((m) => m.repository)}
		>
			Deploy{selected.length ? ` ${selected.length} selected` : '…'}
		</button>
	</div>
</div>

{#if error}<p class="error" role="alert">{error}</p>{/if}

{#if loaded && machines.length === 0}
	<div class="card empty">
		<h2>No machines yet</h2>
		<p class="muted">Add one to get an enrollment token for its agent.</p>
		<button type="button" class="primary" onclick={openAdd}>Add machine</button>
	</div>
{:else if machines.length}
	<div class="strip" role="group" aria-label="Filter by status">
		{#each tiles as tile (tile.key)}
			<button
				type="button"
				class="stat {tile.tone}"
				class:lit={tile.count > 0 && (tile.key === 'offline' || tile.key === 'attention')}
				aria-pressed={filter === tile.key}
				onclick={() => (filter = filter === tile.key ? 'all' : tile.key)}
			>
				<span class="stat-label">{tile.label}</span>
				<span class="stat-count">{tile.count}</span>
			</button>
		{/each}
	</div>

	<div class="table-card">
		<div class="toolbar">
			<div class="search">
				<svg viewBox="0 0 24 24" aria-hidden="true"
					><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" /></svg
				>
				<input
					type="search"
					placeholder="Filter by name or tag"
					aria-label="Filter machines"
					bind:value={query}
				/>
			</div>
			<span class="muted count">
				{#if visible.length !== machines.length}
					Showing {visible.length} of {machines.length}
				{:else}
					{machines.length} machine{machines.length === 1 ? '' : 's'}
				{/if}
			</span>
		</div>
		<table>
			<thead>
				<tr>
					<th class="select"><span class="visually-hidden">Select</span></th>
					<th>Machine</th>
					<th>Status</th>
					<th>Revision</th>
					<th>Last deploy</th>
					<th class="right">Last seen</th>
				</tr>
			</thead>
			<tbody>
				{#each visible as machine (machine.id)}
					<tr data-machine={machine.name} class:selected={selected.includes(machine.name)}>
						<td class="select">
							<input
								type="checkbox"
								aria-label="Select {machine.name}"
								value={machine.name}
								bind:group={selected}
								disabled={!machine.repository || !machine.enrolled_at}
							/>
						</td>
						<td>
							<div class="machine">
								<div class="machine-text">
									<div class="machine-name">
										<a href="/machines/{encodeURIComponent(machine.name)}">{machine.name}</a>
										{#each machine.tags as tag (tag)}<span class="tag">{tag}</span>{/each}
									</div>
									<div class="sub" title={machine.current_system ?? ''}>
										{#if machine.facts}
											<span>{osName(machine.facts.os)}</span>
											<span class="mono">{machine.facts.os_version ?? shortPath(machine.current_system)}</span>
										{:else if machine.enrolled_at}
											<span class="mono">{shortPath(machine.current_system)}</span>
										{:else}
											Not enrolled yet
										{/if}
										{#if machine.facts?.reboot_required}
											<span
												class="chip warn"
												title="The running kernel or initrd differs from the booted one">reboot needed</span
											>
										{/if}
									</div>
								</div>
							</div>
						</td>
						<td><Badge status={machine.status} /></td>
						<td class="mono" data-testid="revision">
							{#if machine.revision}
								{revisionLabel(machine.revision.rev, machine.revision.commit)}
							{:else if machine.current_system}
								<span class="muted" title="Not built by manage.nix">unmanaged</span>
							{:else}<span class="faint">—</span>{/if}
							{#if updates[machine.name]?.behind}
								{@const u = updates[machine.name]}
								<a
									class="chip warn"
									href="/machines/{encodeURIComponent(machine.name)}"
									title="{u.rev} is at {u.commit?.slice(0, 12)}">{u.behind} behind {u.rev}</a
								>
							{/if}
							<a
								class="chip"
								class:warn={updates[machine.name]?.nixpkgs_available === true}
								href="/machines/{encodeURIComponent(machine.name)}"
								>nixpkgs: {updates[machine.name]?.nixpkgs_available === true
									? 'update available'
									: updates[machine.name]?.nixpkgs_available === false
										? 'up to date'
										: 'unknown'}{machine.auto_update.nixpkgs ? ' · auto' : ''}</a
							>
							<a
								class="chip"
								class:warn={Boolean(machine.facts?.flatpak_updates?.length)}
								href="/machines/{encodeURIComponent(machine.name)}"
								>Flatpak: {machine.facts?.flatpak_updates
									? machine.facts.flatpak_updates.length
										? `${machine.facts.flatpak_updates.length} update${machine.facts.flatpak_updates.length === 1 ? '' : 's'}`
										: 'up to date'
									: 'unknown'}{machine.auto_update.flatpak ? ' · auto' : ''}</a
							>
						</td>
						<td>
							{#if machine.last_deployment}
								<div class="deploy">
									<a href="/deploys/{machine.last_deployment.deployment_id}">
										<Badge status={machine.last_deployment.status} />
									</a>
									<span class="muted small" title={exact(machine.last_deployment.created_at)}>
										{ago(machine.last_deployment.created_at, now)}
									</span>
								</div>
							{:else}<span class="faint mono">Never</span>{/if}
						</td>
						<td class="muted right nowrap" title={exact(machine.last_seen_at)}>
							{machine.last_seen_at ? ago(machine.last_seen_at, now) : 'Never'}
						</td>
					</tr>
				{:else}
					<tr>
						<td colspan="6" class="muted no-match">No machines match this filter.</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<DeployDialog bind:this={deployDialog} {machines} />

<Dialog bind:this={addDialog} title="Add machine">
	{#if created}
		<EnrollmentInstructions name={created.machine.name} token={created.enrollment_token} />
	{:else}
		<form class="stack" onsubmit={add}>
			<label>
				Name
				<input
					name="name"
					bind:value={newName}
					placeholder="web-1"
					required
					pattern="[A-Za-z0-9][A-Za-z0-9._\-]*"
					maxlength="63"
					autocomplete="off"
				/>
			</label>
			{#if addError}<p class="error" role="alert">{addError}</p>{/if}
			<div class="actions">
				<button class="primary" type="submit">Add</button>
			</div>
		</form>
	{/if}
</Dialog>

<style>
	svg {
		width: 0.95rem;
		height: 0.95rem;
		flex: none;
		fill: none;
		stroke: currentColor;
		stroke-width: 1.8;
		stroke-linecap: round;
		stroke-linejoin: round;
	}
	.empty {
		padding: 2.5rem 1.5rem;
	}
	.empty h2 {
		margin: 0 0 0.25rem;
	}
	.empty p {
		margin: 0 0 1.25rem;
	}

	/* One ruled strip of counters; each one filters the table. */
	.strip {
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		margin-bottom: 1.25rem;
		border: 1px solid var(--border);
		border-radius: var(--radius);
		background: var(--surface);
		overflow: hidden;
	}
	.stat {
		display: grid;
		justify-content: stretch;
		gap: 0.15rem;
		text-align: left;
		padding: 0.75rem 1rem 0.8rem;
		border: none;
		border-left: 1px solid var(--border);
		border-radius: 0;
		background: none;
		font-weight: inherit;
		box-shadow: inset 0 -2px 0 transparent;
	}
	.stat:first-child {
		border-left: none;
	}
	.stat:hover:not(:disabled) {
		background: var(--hover);
	}
	.stat[aria-pressed='true'] {
		background: var(--surface-2);
		box-shadow: inset 0 -2px 0 var(--ink);
	}
	.stat-label {
		font-family: var(--mono);
		font-size: 0.7rem;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
	}
	.stat-count {
		font-size: 1.6rem;
		font-weight: 500;
		line-height: 1.2;
		font-variant-numeric: tabular-nums;
	}
	/* Only counts that need someone get colour. */
	.lit.bad .stat-count {
		color: var(--bad);
	}
	.lit.warn .stat-count {
		color: var(--warn);
	}

	.toolbar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		padding: 0.6rem 0.9rem;
		border-bottom: 1px solid var(--border);
	}
	.search {
		position: relative;
		width: min(18rem, 100%);
	}
	.search svg {
		position: absolute;
		left: 0.6rem;
		top: 50%;
		translate: 0 -50%;
		color: var(--faint);
		pointer-events: none;
	}
	.search input {
		width: 100%;
		padding-top: 0.35rem;
		padding-bottom: 0.35rem;
		padding-left: 1.9rem;
	}
	.count {
		font-family: var(--mono);
		font-size: 0.78rem;
		white-space: nowrap;
	}

	.select {
		width: 2.5rem;
		padding-right: 0;
	}
	tr.selected {
		background: var(--surface-2);
		box-shadow: inset 2px 0 0 var(--ink);
	}
	.right {
		text-align: right;
	}
	.nowrap {
		white-space: nowrap;
	}
	.small {
		font-size: 0.85rem;
	}
	.faint {
		color: var(--faint);
	}
	.faint.mono {
		font-size: 0.8rem;
	}
	.no-match {
		text-align: center;
		padding: 2.5rem;
	}

	.machine-name {
		display: flex;
		align-items: baseline;
		flex-wrap: wrap;
		gap: 0.4rem;
	}
	.machine-name a {
		color: var(--text);
		font-weight: 600;
	}
	.sub {
		display: flex;
		align-items: baseline;
		flex-wrap: wrap;
		gap: 0.4rem;
		margin-top: 0.1rem;
		color: var(--muted);
		font-size: 0.8rem;
	}
	.sub .mono {
		font-size: 0.76rem;
	}
	.tag {
		font-family: var(--mono);
		font-size: 0.72rem;
		color: var(--muted);
	}
	.tag::before {
		content: '#';
		color: var(--faint);
	}
	.chip {
		display: inline-block;
		padding: 0 0.35rem;
		border-radius: 2px;
		font-family: var(--mono);
		font-size: 0.72rem;
		line-height: 1.6;
	}
	.chip.warn {
		background: var(--warn-bg);
		color: var(--warn);
	}
	td.mono .chip {
		margin-left: 0.4rem;
	}
	.deploy {
		display: flex;
		align-items: baseline;
		gap: 0.6rem;
	}
	td.right {
		font-family: var(--mono);
		font-size: 0.8rem;
	}
	@media (max-width: 52rem) {
		.strip {
			grid-template-columns: repeat(3, minmax(0, 1fr));
		}
		.stat:nth-child(4) {
			border-left: none;
		}
		.stat:nth-child(n + 4) {
			border-top: 1px solid var(--border);
		}
	}
	@media (max-width: 40rem) {
		.toolbar {
			flex-direction: column;
			align-items: stretch;
		}
		.search {
			width: 100%;
		}
	}
</style>
