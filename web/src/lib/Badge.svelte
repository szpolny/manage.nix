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

<span class="badge {tone}" class:pulse={tone === 'busy'} data-status={status}>{words(status)}</span>

<style>
	.badge {
		display: inline-flex;
		align-items: center;
		gap: 0.45rem;
		font-family: var(--mono);
		font-size: 0.8rem;
		font-weight: 500;
		white-space: nowrap;
		color: var(--tone-text);
	}
	.badge::before {
		content: '';
		width: 0.5rem;
		height: 0.5rem;
		flex: none;
		background: var(--tone);
	}
	.ok {
		--tone: var(--ok-dot);
		--tone-text: var(--ok);
	}
	.bad {
		--tone: var(--bad-dot);
		--tone-text: var(--bad);
	}
	.warn {
		--tone: var(--warn-dot);
		--tone-text: var(--warn);
	}
	.busy {
		--tone: var(--busy-dot);
		--tone-text: var(--busy);
	}
	.idle {
		--tone: transparent;
		--tone-text: var(--muted);
	}
	.idle::before {
		box-shadow: inset 0 0 0 1.5px var(--idle-dot);
	}
	.pulse::before {
		animation: blink 1s steps(2, jump-none) infinite;
	}
	@keyframes blink {
		50% {
			opacity: 0.2;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.pulse::before {
			animation: none;
		}
	}
</style>
