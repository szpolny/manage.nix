<script lang="ts">
	import { onMount } from 'svelte';
	import { api, errorMessage } from '#lib/api.ts';
	import Badge from '#lib/Badge.svelte';
	import DeployDialog from '#lib/DeployDialog.svelte';
	import Dialog from '#lib/Dialog.svelte';
	import EnrollmentInstructions from '#lib/EnrollmentInstructions.svelte';
	import { ago, exact, revisionLabel, shortPath } from '#lib/format.ts';
	import type { CreatedMachine, Machine } from '#lib/types.ts';

	let machines: Machine[] = $state([]);
	let loaded = $state(false);
	let error = $state('');
	let selected: string[] = $state([]);
	let now = $state(Date.now());

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

	onMount(() => {
		refresh();
		const timer = setInterval(refresh, 5000);
		return () => clearInterval(timer);
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

	let counts = $derived({
		online: machines.filter((m) => m.status === 'online').length,
		offline: machines.filter((m) => m.status === 'offline').length,
		pending: machines.filter((m) => m.status === 'pending').length
	});
</script>

<svelte:head><title>Machines · manage.nix</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Machines</h1>
		{#if machines.length}
			<p class="muted summary">
				{counts.online} online · {counts.offline} offline · {counts.pending} waiting to enroll
			</p>
		{/if}
	</div>
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
		<p>No machines yet. Add one to get an enrollment token for its agent.</p>
		<button type="button" class="primary" onclick={openAdd}>Add machine</button>
	</div>
{:else if machines.length}
	<div class="table-card">
		<table>
			<thead>
				<tr>
					<th class="select"><span class="visually-hidden">Select</span></th>
					<th>Name</th>
					<th>Status</th>
					<th>Revision</th>
					<th>Last deploy</th>
					<th>System</th>
					<th>Last seen</th>
				</tr>
			</thead>
			<tbody>
				{#each machines as machine (machine.id)}
					<tr data-machine={machine.name}>
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
							<a href="/machines/{encodeURIComponent(machine.name)}"><strong>{machine.name}</strong></a>
							{#each machine.tags as tag (tag)}<span class="tag">{tag}</span>{/each}
						</td>
						<td><Badge status={machine.status} /></td>
						<td class="mono" data-testid="revision">
							{#if machine.revision}
								{revisionLabel(machine.revision.rev, machine.revision.commit)}
							{:else if machine.current_system}
								<span class="muted" title="Not built by manage.nix">unmanaged</span>
							{:else}—{/if}
						</td>
						<td>
							{#if machine.last_deployment}
								<a href="/deploys/{machine.last_deployment.deployment_id}">
									<Badge status={machine.last_deployment.status} />
								</a>
								<span class="muted" title={exact(machine.last_deployment.created_at)}>
									{ago(machine.last_deployment.created_at, now)}
								</span>
							{:else}<span class="muted">never</span>{/if}
						</td>
						<td>
							<span class="mono" title={machine.current_system ?? ''}>
								{machine.facts?.os_version ?? shortPath(machine.current_system)}
							</span>
							{#if machine.facts?.reboot_required}
								<span class="reboot" title="The running kernel or initrd differs from the booted one"
									>reboot needed</span
								>
							{/if}
						</td>
						<td class="muted" title={exact(machine.last_seen_at)}>
							{machine.last_seen_at ? ago(machine.last_seen_at, now) : 'never'}
						</td>
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
	.summary {
		margin: 0.25rem 0 0;
	}
	.empty {
		text-align: center;
		padding: 2.5rem;
	}
	.select {
		width: 2rem;
		padding-right: 0;
	}
	.tag {
		display: inline-block;
		margin-left: 0.4rem;
		padding: 0 0.4rem;
		border-radius: 4px;
		background: var(--idle-bg);
		color: var(--muted);
		font-size: 0.75rem;
	}
	.reboot {
		display: block;
		color: var(--warn);
		font-size: 0.8rem;
	}
	.visually-hidden {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
	}
</style>
