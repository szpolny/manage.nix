<script lang="ts">
	import { onMount } from 'svelte';
	import { api, errorMessage } from '#lib/api.ts';
	import { exact } from '#lib/format.ts';
	import type { Repository } from '#lib/types.ts';

	let repositories: Repository[] = $state([]);
	let error = $state('');
	let name = $state('');
	let url = $state('');

	async function refresh() {
		try {
			repositories = await api.repositories();
		} catch (e) {
			error = errorMessage(e);
		}
	}

	onMount(refresh);

	async function add(event: SubmitEvent) {
		event.preventDefault();
		error = '';
		try {
			await api.addRepository(name.trim(), url.trim());
			name = url = '';
			await refresh();
		} catch (e) {
			error = errorMessage(e);
		}
	}
</script>

<svelte:head><title>Repositories · manage.nix</title></svelte:head>

<div class="page-header"><h1>Repositories</h1></div>

<p class="muted intro">
	Git repositories with a flake whose <code>nixosConfigurations</code> the server builds machines from.
</p>

{#if repositories.length}
	<div class="table-card">
		<table>
			<thead><tr><th>Name</th><th>URL</th><th>Added</th></tr></thead>
			<tbody>
				{#each repositories as r (r.id)}
					<tr>
						<td><strong>{r.name}</strong></td>
						<td class="mono">{r.url}</td>
						<td class="muted">{exact(r.created_at)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<form class="card form-row add" onsubmit={add}>
	<label>
		Name
		<input name="name" bind:value={name} placeholder="infra" required autocomplete="off" />
	</label>
	<label class="grow">
		Git URL
		<input
			name="url"
			bind:value={url}
			placeholder="https://git.example.com/infra.git"
			required
			autocomplete="off"
		/>
	</label>
	<button class="primary" type="submit">Add repository</button>
</form>
{#if error}<p class="error" role="alert">{error}</p>{/if}

<style>
	.intro {
		margin-top: -0.5rem;
	}
	.add {
		margin-top: 1rem;
	}
	.grow {
		flex: 1;
		min-width: 16rem;
	}
</style>
