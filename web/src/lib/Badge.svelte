<!-- A status pill, coloured by what the status means. -->
<script lang="ts">
	import { words } from './format.ts';

	let { status }: { status: string | null | undefined } = $props();

	const tones: Record<string, string> = {
		online: 'ok',
		succeeded: 'ok',
		offline: 'bad',
		failed: 'bad',
		rolled_back: 'warn',
		pending: 'idle',
		queued: 'idle',
		building: 'busy',
		activating: 'busy'
	};
	let tone = $derived(tones[status ?? ''] ?? 'idle');
</script>

<span class="badge {tone}" data-status={status}>{words(status)}</span>

<style>
	.badge {
		display: inline-block;
		padding: 0.05rem 0.55rem;
		border-radius: 999px;
		font-size: 0.8rem;
		font-weight: 600;
		white-space: nowrap;
	}
	.ok {
		color: var(--ok);
		background: var(--ok-bg);
	}
	.bad {
		color: var(--bad);
		background: var(--bad-bg);
	}
	.warn {
		color: var(--warn);
		background: var(--warn-bg);
	}
	.busy {
		color: var(--busy);
		background: var(--busy-bg);
	}
	.idle {
		color: var(--idle);
		background: var(--idle-bg);
	}
</style>
