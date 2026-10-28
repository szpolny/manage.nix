<!-- A modal dialog. Call `show()` to open it; Escape or `close()` closes it. -->
<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		title,
		children,
		onclose
	}: { title: string; children: Snippet; onclose?: () => void } = $props();

	let dialog: HTMLDialogElement;

	export function show() {
		dialog.showModal();
	}

	export function close() {
		dialog.close();
	}
</script>

<dialog bind:this={dialog} aria-label={title} {onclose}>
	<header>
		<h2>{title}</h2>
		<button type="button" class="close" aria-label="Close" onclick={close}>✕</button>
	</header>
	{@render children()}
</dialog>

<style>
	dialog {
		border: 1px solid var(--border);
		border-radius: var(--radius);
		background: var(--surface);
		color: var(--text);
		padding: 1.25rem 1.5rem 1.5rem;
		width: min(36rem, calc(100vw - 2rem));
	}
	dialog::backdrop {
		background: rgb(0 0 0 / 0.4);
	}
	header {
		display: flex;
		justify-content: space-between;
		align-items: start;
	}
	.close {
		border: none;
		background: none;
		padding: 0.1rem 0.3rem;
		color: var(--muted);
	}
</style>
