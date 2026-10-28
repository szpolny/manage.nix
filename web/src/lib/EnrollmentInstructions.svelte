<!-- How to enroll a machine with a one-time token. -->
<script lang="ts">
	import { exact } from './format.ts';
	import type { EnrollmentToken } from './types.ts';

	let { name, token }: { name: string; token: EnrollmentToken } = $props();
	let copied = $state(false);

	async function copy() {
		await navigator.clipboard.writeText(token.token);
		copied = true;
	}
</script>

<div class="stack">
	<p>
		The one-time enrollment token for <strong>{name}</strong>, valid until
		{exact(token.expires_at)}. It is shown only once.
	</p>
	<div class="token">
		<code data-testid="enrollment-token">{token.token}</code>
		<button type="button" onclick={copy}>{copied ? 'Copied' : 'Copy'}</button>
	</div>
	<p class="muted">
		On the machine, write it to a file and point the agent at it:
	</p>
	<pre class="log">services.manage-nix.agent = &#123;
  enable = true;
  server.url = "{location.origin}";
  enrollmentTokenFile = "/run/secrets/mnix-enrollment-token";
&#125;;</pre>
</div>

<style>
	.token {
		display: flex;
		gap: 0.5rem;
		align-items: center;
	}
	.token code {
		flex: 1;
		background: var(--idle-bg);
		border-radius: 6px;
		padding: 0.45rem 0.6rem;
		overflow-wrap: anywhere;
	}
	p {
		margin: 0;
	}
</style>
