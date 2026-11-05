# Called by nix-darwin activation; accepts a file path for unprivileged tests.
set -eu
hosts=$1
enabled=$2
updated=$(mktemp)
trap 'rm -f "$updated"' EXIT

# Preserve other entries, including unmarked entries for the same hostname.
awk '$0 !~ /# manage-nix local HTTPS$/ { print }' "$hosts" > "$updated"
if [ "$enabled" = true ]; then
    printf '%s\n' '127.0.0.1 manage.nix # manage-nix local HTTPS' >> "$updated"
fi
if ! cmp -s "$hosts" "$updated"; then
    # Write through the existing file to preserve its permissions and ownership.
    cat "$updated" > "$hosts"
    echo updated
fi
