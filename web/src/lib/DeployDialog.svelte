<!-- Choose a revision, machines, and a mode, then start a deployment and
     follow it on its page. -->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { api, errorMessage } from './api.ts';
	import Dialog from './Dialog.svelte';
	import type { ActivationMode, Machine } from './types.ts';

	let { machines }: { machines: Machine[] } = $props();

	let dialog: Dialog;
	let selected: string[] = $state([]);
	let rev = $state('');
	let mode: ActivationMode = $state('switch');
	let error = $state('');
	let busy = $state(false);

	let deployable = $derived(machines.filter((m) => m.repository && m.enrolled_at));

	export function open(preselected: string[] = [], presetRev?: string) {
		selected = preselected.filter((name) => deployable.some((m) => m.name === name));
		if (presetRev !== undefined) rev = presetRev;
		error = '';
		dialog.show();
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = '';
		try {
			const deployment = await api.deploy(selected, rev.trim() || undefined, mode);
			dialog.close();
			await goto(`/deploys/${deployment.id}`);
		} catch (e) {
			error = errorMessage(e);
		} finally {
			busy = false;
		}
	}
</script>

<Dialog bind:this={dialog} title="Deploy">
	<form class="stack" onsubmit={submit}>
		<label>
			Revision
			<input
				name="rev"
				bind:value={rev}
				placeholder="HEAD (the default branch)"
				autocomplete="off"
			/>
		</label>
		<fieldset>
			<legend>Machines</legend>
			{#each deployable as machine (machine.id)}
				<label class="check">
					<input type="checkbox" value={machine.name} bind:group={selected} />
					{machine.name}
					<span class="muted">{machine.repository}#{machine.configuration}</span>
				</label>
			{:else}
				<p class="muted">
					No machine can be deployed yet: enroll one and choose its repository on its page.
				</p>
			{/each}
		</fieldset>
		<label>
			Mode
			<select name="mode" bind:value={mode}>
				<option value="switch">switch: activate now and on boot</option>
				<option value="test">test: activate now only</option>
				<option value="boot">boot: activate on next boot</option>
			</select>
		</label>
		{#if error}<p class="error" role="alert">{error}</p>{/if}
		<div class="actions">
			<button class="primary" type="submit" disabled={busy || selected.length === 0}>
				Deploy {selected.length || ''}
				{selected.length === 1 ? 'machine' : 'machines'}
			</button>
		</div>
	</form>
</Dialog>

<style>
	fieldset {
		border: 1px solid var(--border);
		border-radius: 6px;
		padding: 0.5rem 0.75rem 0.75rem;
		display: grid;
		gap: 0.35rem;
		max-height: 16rem;
		overflow: auto;
	}
	legend {
		font-size: 0.9rem;
		font-weight: 500;
		padding: 0 0.25rem;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-weight: 400;
	}
	.check .muted {
		font-size: 0.85rem;
	}
</style>
