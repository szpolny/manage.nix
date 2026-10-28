<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api, errorMessage } from '#lib/api.ts';
	import Badge from '#lib/Badge.svelte';
	import DeployDialog from '#lib/DeployDialog.svelte';
	import Dialog from '#lib/Dialog.svelte';
	import EnrollmentInstructions from '#lib/EnrollmentInstructions.svelte';
	import { ago, exact, revisionLabel, shortPath } from '#lib/format.ts';
	import type {
		ClosureDiff,
		EnrollmentToken,
		GenerationView,
		Machine,
		MachineDeployment,
		Repository
	} from '#lib/types.ts';

	let name = $derived(page.params.name ?? '');

	let machine: Machine | null = $state(null);
	let generations: GenerationView[] = $state([]);
	let history: MachineDeployment[] = $state([]);
	let repositories: Repository[] = $state([]);
	let error = $state('');
	let now = $state(Date.now());

	// Configuration and tags forms.
	let repository = $state('');
	let configuration = $state('');
	let tags = $state('');
	let formError = $state('');
	let saved = $state('');

	// Generation diff.
	let from: number | null = $state(null);
	let to: number | null = $state(null);
	let diff: ClosureDiff | null = $state(null);
	let diffError = $state('');
	let diffing = $state(false);

	let deployDialog: DeployDialog | undefined = $state();
	let tokenDialog: Dialog;
	let token: EnrollmentToken | null = $state(null);

	async function load(first: boolean) {
		try {
			const [m, g, h] = await Promise.all([
				api.machine(name),
				api.generations(name),
				api.machineDeployments(name)
			]);
			machine = m;
			generations = g;
			history = h;
			now = Date.now();
			if (first) {
				repository = m.repository ?? '';
				configuration = m.configuration ?? '';
				tags = m.tags.join(', ');
				// Compare the running system with the generation before it.
				const running = g.findIndex((gen) => gen.running);
				const index = running === -1 ? 0 : running;
				to = g[index]?.number ?? null;
				from = g[index + 1]?.number ?? null;
				repositories = await api.repositories();
			}
			error = '';
		} catch (e) {
			error = errorMessage(e);
		}
	}

	$effect(() => {
		// Reload when the route's machine changes, and every 5 seconds.
		void name;
		machine = null;
		diff = null;
		load(true);
		const timer = setInterval(() => load(false), 5000);
		return () => clearInterval(timer);
	});

	async function saveConfiguration(event: SubmitEvent) {
		event.preventDefault();
		formError = saved = '';
		try {
			machine = await api.configure(name, repository, configuration.trim() || undefined);
			configuration = machine.configuration ?? '';
			saved = 'configuration';
		} catch (e) {
			formError = errorMessage(e);
		}
	}

	async function saveTags(event: SubmitEvent) {
		event.preventDefault();
		formError = saved = '';
		try {
			const list = tags
				.split(/[\s,]+/)
				.map((t) => t.trim())
				.filter(Boolean);
			machine = await api.setTags(name, list);
			tags = machine.tags.join(', ');
			saved = 'tags';
		} catch (e) {
			formError = errorMessage(e);
		}
	}

	async function compare() {
		if (from === null || to === null) return;
		diffing = true;
		diffError = '';
		diff = null;
		try {
			diff = await api.diff(name, from, to);
		} catch (e) {
			diffError = errorMessage(e);
		} finally {
			diffing = false;
		}
	}

	async function rollback() {
		if (!confirm(`Roll ${name} back to its previous generation?`)) return;
		try {
			const deployment = await api.rollback(name);
			await goto(`/deploys/${deployment.id}`);
		} catch (e) {
			error = errorMessage(e);
		}
	}

	async function newToken() {
		try {
			token = await api.enrollmentToken(name);
			tokenDialog.show();
		} catch (e) {
			error = errorMessage(e);
		}
	}
</script>

<svelte:head><title>{name} · manage.nix</title></svelte:head>

<div class="page-header">
	<div class="title">
		<a href="/" class="muted">Machines</a>
		<span class="muted">/</span>
		<h1>{name}</h1>
		{#if machine}<Badge status={machine.status} />{/if}
	</div>
	{#if machine}
		<div class="actions">
			<button type="button" onclick={newToken}>
				{machine.enrolled_at ? 'Re-enroll…' : 'New enrollment token'}
			</button>
			<button type="button" onclick={rollback} disabled={!machine.enrolled_at}>Roll back</button>
			<button
				type="button"
				class="primary"
				onclick={() => deployDialog?.open([name])}
				disabled={!machine.repository || !machine.enrolled_at}>Deploy…</button
			>
		</div>
	{/if}
</div>

{#if error}<p class="error" role="alert">{error}</p>{/if}

{#if machine}
	<div class="grid">
		<section class="card">
			<h2>System</h2>
			{#if machine.facts}
				<dl class="facts">
					<dt>Revision</dt>
					<dd class="mono" data-testid="revision">
						{#if machine.revision}
							<a href="/deploys/{machine.revision.deployment_id}">
								{revisionLabel(machine.revision.rev, machine.revision.commit)}
							</a>
							{#if machine.revision.repository}
								<span class="muted">from {machine.revision.repository}</span>
							{/if}
						{:else}
							<span class="muted">not built by manage.nix</span>
						{/if}
					</dd>
					<dt>Running</dt>
					<dd class="mono" title={machine.facts.current_system}>
						{shortPath(machine.facts.current_system)}
					</dd>
					<dt>Booted</dt>
					<dd class="mono" title={machine.facts.booted_system ?? ''}>
						{shortPath(machine.facts.booted_system)}
					</dd>
					<dt>Reboot needed</dt>
					<dd>
						{#if machine.facts.reboot_required}
							<strong class="warn">yes</strong>: the running kernel or initrd differs from the booted
							one
						{:else}no{/if}
					</dd>
					<dt>NixOS</dt>
					<dd>{machine.facts.nixos_version ?? '—'}</dd>
					<dt>Kernel</dt>
					<dd>{machine.facts.kernel}</dd>
					<dt>Hostname</dt>
					<dd>{machine.facts.hostname}</dd>
					<dt>Agent</dt>
					<dd>{machine.agent_version ?? '—'}</dd>
					<dt>Last seen</dt>
					<dd title={exact(machine.last_seen_at)}>{ago(machine.last_seen_at, now)}</dd>
				</dl>
			{:else}
				<p class="muted">
					{machine.enrolled_at
						? 'The agent has not reported yet.'
						: 'Waiting for the agent to enroll.'}
				</p>
			{/if}
		</section>

		<section class="card stack">
			<h2>Configuration</h2>
			<form class="stack" onsubmit={saveConfiguration}>
				<div class="form-row">
					<label>
						Repository
						<select name="repository" bind:value={repository} required>
							<option value="" disabled>Choose…</option>
							{#each repositories as r (r.id)}
								<option value={r.name}>{r.name}</option>
							{/each}
						</select>
					</label>
					<label class="grow">
						nixosConfigurations.
						<input name="configuration" bind:value={configuration} placeholder={name} />
					</label>
					<button type="submit">Save</button>
				</div>
				{#if repositories.length === 0}
					<p class="muted small">
						<a href="/repositories">Add a repository</a> to build this machine from.
					</p>
				{/if}
			</form>
			<form class="form-row" onsubmit={saveTags}>
				<label class="grow">
					Tags
					<input name="tags" bind:value={tags} placeholder="web, eu" />
				</label>
				<button type="submit">Save tags</button>
			</form>
			{#if formError}<p class="error" role="alert">{formError}</p>{/if}
			{#if saved}<p class="muted small" role="status">Saved {saved}.</p>{/if}
		</section>
	</div>

	<section class="section">
		<h2>Generations</h2>
		{#if generations.length}
			<div class="table-card">
				<table>
					<thead>
						<tr>
							<th>From</th>
							<th>To</th>
							<th>#</th>
							<th>Created</th>
							<th>Revision</th>
							<th>System</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each generations as g (g.number)}
							<tr>
								<td>
									<input
										type="radio"
										name="from"
										aria-label="Compare from generation {g.number}"
										value={g.number}
										bind:group={from}
									/>
								</td>
								<td>
									<input
										type="radio"
										name="to"
										aria-label="Compare to generation {g.number}"
										value={g.number}
										bind:group={to}
									/>
								</td>
								<td>{g.number}</td>
								<td title={exact(g.created_at)}>{ago(g.created_at, now)}</td>
								<td class="mono">
									{#if g.revision}
										<a href="/deploys/{g.revision.deployment_id}"
											>{revisionLabel(g.revision.rev, g.revision.commit)}</a
										>
									{:else}<span class="muted">—</span>{/if}
								</td>
								<td class="mono" title={g.store_path}>{shortPath(g.store_path)}</td>
								<td class="flags">
									{#if g.running}<span class="flag running">running</span>{/if}
									{#if g.current}<span class="flag">boot default</span>{/if}
									{#if g.booted}<span class="flag">booted</span>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div class="diff-actions">
				<button
					type="button"
					onclick={compare}
					disabled={from === null || to === null || from === to || diffing}
				>
					{diffing ? 'Comparing…' : `Compare ${from ?? '?'} → ${to ?? '?'}`}
				</button>
			</div>
			{#if diffError}<p class="error" role="alert">{diffError}</p>{/if}
			{#if diff}
				<pre class="log" data-testid="diff">{diff.diff.trim() || 'The closures are identical.'}</pre>
			{/if}
		{:else}
			<p class="muted">No generations reported.</p>
		{/if}
	</section>

	<section class="section">
		<h2>Recent deploys</h2>
		{#if history.length}
			<div class="table-card">
				<table>
					<thead>
						<tr>
							<th>Status</th>
							<th>Kind</th>
							<th>Revision</th>
							<th>By</th>
							<th>When</th>
							<th>Message</th>
						</tr>
					</thead>
					<tbody>
						{#each history as d (d.deployment_id)}
							<tr>
								<td><a href="/deploys/{d.deployment_id}"><Badge status={d.status} /></a></td>
								<td>{d.kind} <span class="muted">({d.mode})</span></td>
								<td class="mono">{d.kind === 'rollback' ? '—' : revisionLabel(d.rev, d.commit)}</td>
								<td>{d.requested_by}</td>
								<td title={exact(d.created_at)}>{ago(d.created_at, now)}</td>
								<td class="muted">{d.message ?? ''}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<p class="muted">Never deployed.</p>
		{/if}
	</section>

	<DeployDialog bind:this={deployDialog} machines={[machine]} />
{/if}

<Dialog bind:this={tokenDialog} title="Enrollment token">
	{#if token}<EnrollmentInstructions {name} {token} />{/if}
</Dialog>

<style>
	.title {
		display: flex;
		align-items: center;
		gap: 0.6rem;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(22rem, 1fr));
		gap: 1rem;
		align-items: start;
	}
	.section {
		margin-top: 1.75rem;
	}
	.grow {
		flex: 1;
	}
	.small {
		font-size: 0.85rem;
		margin: 0;
	}
	.warn {
		color: var(--warn);
	}
	.flags {
		white-space: nowrap;
	}
	.flag {
		display: inline-block;
		margin-right: 0.3rem;
		padding: 0 0.4rem;
		border-radius: 4px;
		background: var(--idle-bg);
		color: var(--muted);
		font-size: 0.75rem;
	}
	.flag.running {
		background: var(--ok-bg);
		color: var(--ok);
	}
	.diff-actions {
		margin: 0.75rem 0;
	}
	h2 :global(+ p) {
		margin-top: 0;
	}
</style>
