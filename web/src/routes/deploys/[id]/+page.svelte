<script lang="ts">
	import { tick } from 'svelte';
	import { page } from '$app/state';
	import { api, errorMessage } from '#lib/api.ts';
	import Badge from '#lib/Badge.svelte';
	import { exact, revisionLabel, shortPath, words } from '#lib/format.ts';
	import type { Deployment, LogEntry } from '#lib/types.ts';

	let id = $derived(page.params.id ?? '');

	let deployment: Deployment | null = $state(null);
	let logs: LogEntry[] = $state([]);
	let error = $state('');
	let live = $state(false);
	let filter = $state('');
	let follow = $state(true);
	let logElement: HTMLPreElement | undefined = $state();

	$effect(() => {
		const deploymentId = id;
		deployment = null;
		logs = [];
		error = '';
		api.deployment(deploymentId).then(
			(d) => (deployment ??= d),
			(e) => (error = errorMessage(e))
		);
		// The server sends the deployment whenever it changes, its log
		// entries, and `end` once it has finished.
		const events = new EventSource(api.deploymentEvents(deploymentId));
		live = true;
		events.addEventListener('deployment', (e) => {
			deployment = JSON.parse((e as MessageEvent).data);
		});
		events.addEventListener('log', (e) => {
			logs.push(JSON.parse((e as MessageEvent).data));
			scroll();
		});
		events.addEventListener('end', () => {
			events.close();
			live = false;
		});
		return () => {
			events.close();
			live = false;
		};
	});

	async function scroll() {
		if (!follow) return;
		await tick();
		logElement?.scrollTo({ top: logElement.scrollHeight });
	}

	let machines = $derived([
		...new Set(logs.map((l) => l.machine).filter((m): m is string => m !== null))
	]);
	let shown = $derived(filter ? logs.filter((l) => l.machine === filter) : logs);

	/** The log as text, each line prefixed with its machine. */
	function render(entries: LogEntry[]) {
		return entries
			.map((entry) => {
				const prefix = entry.machine ? `[${entry.machine}] ` : '';
				return entry.text
					.replace(/\n$/, '')
					.split('\n')
					.map((line) => prefix + line)
					.join('\n');
			})
			.join('\n');
	}
</script>

<svelte:head><title>Deploy · manage.nix</title></svelte:head>

<div class="page-header">
	<div class="title">
		<a href="/deploys" class="muted">Deploys</a>
		<span class="muted">/</span>
		<h1>
			{#if deployment}
				{deployment.kind === 'rollback'
					? 'Rollback'
					: revisionLabel(deployment.rev, deployment.commit)}
			{:else}
				{id.slice(0, 8)}
			{/if}
		</h1>
		{#if deployment}<Badge status={deployment.status} />{/if}
		{#if live && deployment && !['succeeded', 'failed'].includes(deployment.status)}
			<span class="live" title="Following live">● live</span>
		{/if}
	</div>
</div>

{#if error}<p class="error" role="alert">{error}</p>{/if}

{#if deployment}
	<section class="card">
		<dl class="facts">
			{#if deployment.kind === 'deploy'}
				<dt>Revision</dt>
				<dd class="mono">
					{deployment.rev ?? '—'}
					{#if deployment.commit}<span class="muted">→ {deployment.commit}</span>{/if}
				</dd>
				<dt>Repository</dt>
				<dd>{deployment.repository ?? '—'}</dd>
			{/if}
			<dt>Mode</dt>
			<dd>{deployment.mode}</dd>
			<dt>Requested by</dt>
			<dd>{deployment.requested_by}</dd>
			<dt>Created</dt>
			<dd>{exact(deployment.created_at)}</dd>
			<dt>Finished</dt>
			<dd>{deployment.finished_at ? exact(deployment.finished_at) : '—'}</dd>
			{#if deployment.message}
				<dt>Message</dt>
				<dd class="bad">{deployment.message}</dd>
			{/if}
		</dl>
	</section>

	<section class="section">
		<h2>Machines</h2>
		<div class="table-card">
			<table>
				<thead>
					<tr>
						<th>Machine</th>
						<th>Status</th>
						<th>Phase</th>
						<th>System</th>
						<th>Message</th>
					</tr>
				</thead>
				<tbody>
					{#each deployment.machines as m (m.machine_id)}
						<tr data-machine={m.machine}>
							<td><a href="/machines/{encodeURIComponent(m.machine)}">{m.machine}</a></td>
							<td><Badge status={m.status} /></td>
							<td class="muted">{m.status === 'activating' ? words(m.phase) : ''}</td>
							<td class="mono" title={m.store_path ?? ''}>{shortPath(m.store_path)}</td>
							<td>
								{m.message ?? ''}
								{#if m.report?.failed_checks.length}
									<div class="muted">failed: {m.report.failed_checks.join(', ')}</div>
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</section>

	<section class="section">
		<div class="log-header">
			<h2>Log</h2>
			<div class="actions">
				{#if machines.length > 1}
					<select bind:value={filter} aria-label="Show the log of">
						<option value="">All machines</option>
						{#each machines as machine (machine)}<option value={machine}>{machine}</option>{/each}
					</select>
				{/if}
				<label class="follow"><input type="checkbox" bind:checked={follow} /> Follow</label>
			</div>
		</div>
		<pre class="log" bind:this={logElement} data-testid="log">{shown.length
				? render(shown)
				: live
					? 'Waiting for output…'
					: 'No output.'}</pre>
	</section>
{/if}

<style>
	.title {
		display: flex;
		align-items: center;
		gap: 0.6rem;
	}
	.live {
		color: var(--busy);
		font-size: 0.85rem;
		font-weight: 600;
	}
	.section {
		margin-top: 1.5rem;
	}
	.bad {
		color: var(--bad);
	}
	.log-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: 0.5rem;
	}
	.log-header h2 {
		margin: 0;
	}
	.follow {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		font-weight: 400;
	}
	pre.log {
		max-height: 70vh;
		min-height: 8rem;
	}
</style>
