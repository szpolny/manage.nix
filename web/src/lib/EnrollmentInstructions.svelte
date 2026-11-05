<!-- How to enroll a machine with a one-time token. -->
<script lang="ts">
	import { exact } from './format.ts';
	import type { EnrollmentToken } from './types.ts';

	let { name, token }: { name: string; token: EnrollmentToken } = $props();
	let copied = $state(false);
	let copyFailed = $state(false);

	async function copy() {
		copyFailed = false;
		if (navigator.clipboard) {
			try {
				await navigator.clipboard.writeText(token.token);
				copied = true;
				return;
			} catch {
				// Fall through when browser clipboard permission is unavailable.
			}
		}
		// HTTP on a custom loopback hostname has no Clipboard API.
		const input = document.createElement('textarea');
		input.value = token.token;
		input.style.position = 'fixed';
		input.style.opacity = '0';
		input.readOnly = true;
		document.body.append(input);
		try {
			input.select();
			copied = document.execCommand('copy');
		} catch {
			copied = false;
		} finally {
			input.remove();
		}
		copyFailed = !copied;
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
	{#if copyFailed}
		<p role="status">Copy was blocked. Select the token above and copy it manually.</p>
	{/if}
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
