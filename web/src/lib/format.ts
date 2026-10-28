// Display helpers.

const units: [Intl.RelativeTimeFormatUnit, number][] = [
	['day', 86400],
	['hour', 3600],
	['minute', 60],
	['second', 1]
];
const relative = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

/** "3 minutes ago", or "—" for null. */
export function ago(timestamp: string | null | undefined, now = Date.now()): string {
	if (!timestamp) return '—';
	const seconds = Math.round((Date.parse(timestamp) - now) / 1000);
	for (const [unit, size] of units) {
		if (Math.abs(seconds) >= size || unit === 'second') {
			return relative.format(Math.round(seconds / size), unit);
		}
	}
	return timestamp;
}

/** A full local date and time, for tooltips. */
export function exact(timestamp: string | null | undefined): string {
	return timestamp ? new Date(timestamp).toLocaleString() : '';
}

/** `/nix/store/abc…-nixos-system-web-1-26.11` → `abc1234…-nixos-system-web-1-26.11`. */
export function shortPath(path: string | null | undefined): string {
	if (!path) return '—';
	const name = path.replace(/^\/nix\/store\//, '');
	const dash = name.indexOf('-');
	return dash === 32 ? `${name.slice(0, 7)}…${name.slice(dash)}` : name;
}

export function shortCommit(commit: string | null | undefined): string {
	return commit ? commit.slice(0, 8) : '—';
}

/** `main @ 1a2b3c4d`, or just the commit. */
export function revisionLabel(rev: string | null | undefined, commit: string | null | undefined) {
	if (!commit) return rev ?? '—';
	if (!rev || commit.startsWith(rev)) return shortCommit(commit);
	return `${rev} @ ${shortCommit(commit)}`;
}

/** `rolled_back` → `rolled back`. */
export function words(value: string | null | undefined): string {
	return value ? value.replaceAll('_', ' ') : '—';
}
