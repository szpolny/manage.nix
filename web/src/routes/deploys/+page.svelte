<script lang="ts">
	import { onMount } from 'svelte';
	import { api, errorMessage } from '#lib/api.ts';
	import Badge from '#lib/Badge.svelte';
	import { ago, exact, revisionLabel } from '#lib/format.ts';
	import type { Deployment } from '#lib/types.ts';

	let deployments: Deployment[] = $state([]);
	let loaded = $state(false);
	let error = $state('');
	let now = $state(Date.now());

	async function refresh() {
		try {
			deployments = await api.deployments(100);
			now = Date.now();
			error = '';
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loaded = true;
		}
	}

	onMount(() => {
		refresh();
		const timer = setInterval(refresh, 5000);
		return () => clearInterval(timer);
	});

	function duration(d: Deployment) {
		if (!d.finished_at) return '';
		const seconds = Math.round((Date.parse(d.finished_at) - Date.parse(d.created_at)) / 1000);
		return seconds < 90 ? `${seconds}s` : `${Math.round(seconds / 60)} min`;
	}
</script>

<svelte:head><title>Deploys · manage.nix</title></svelte:head>

<div class="page-header"><h1>Deploys</h1></div>

{#if error}<p class="error" role="alert">{error}</p>{/if}

{#if loaded && deployments.length === 0}
	<div class="card"><p class="muted">Nothing has been deployed yet.</p></div>
{:else if deployments.length}
	<div class="table-card">
		<table>
			<thead>
				<tr>
					<th>Status</th>
					<th>What</th>
					<th>Machines</th>
					<th>By</th>
					<th>Started</th>
					<th>Took</th>
				</tr>
			</thead>
			<tbody>
				{#each deployments as d (d.id)}
					<tr>
						<td><a href="/deploys/{d.id}"><Badge status={d.status} /></a></td>
						<td>
							<a href="/deploys/{d.id}">
								{#if d.kind !== 'deploy'}
									{d.kind}
								{:else}
									<span class="mono">{revisionLabel(d.rev, d.commit)}</span>
								{/if}
							</a>
							{#if d.repository}<span class="muted">from {d.repository}</span>{/if}
							{#if d.kind !== 'reboot' && d.mode !== 'switch'}<span class="muted"
									>({d.mode})</span
								>{/if}
						</td>
						<td>
							{#each d.machines as m, i (m.machine_id)}{i ? ', ' : ''}<a
									href="/machines/{encodeURIComponent(m.machine)}"
									class:bad={m.status === 'failed' || m.status === 'rolled_back'}>{m.machine}</a
								>{/each}
						</td>
						<td>{d.requested_by}</td>
						<td class="muted" title={exact(d.created_at)}>{ago(d.created_at, now)}</td>
						<td class="muted">{duration(d)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<style>
	.bad {
		color: var(--bad);
	}
</style>
