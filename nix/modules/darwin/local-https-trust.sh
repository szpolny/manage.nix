# Run from privileged activation, where macOS can authorize admin trust changes.
# Caddy's background launchd process cannot reliably install trust settings.
set -eu
security=$1
root=$2
remaining=${3:-60}

while [ ! -s "$root" ]; do
    if [ "$remaining" -le 0 ]; then
        echo "manage.nix: local HTTPS root was not generated; check /var/lib/manage-nix-https/proxy.log" >&2
        exit 1
    fi
    sleep 1
    remaining=$((remaining - 1))
done

# Always ensure admin-domain SSL trust. Presence in System.keychain, or
# verification using root's personal trust settings, is not sufficient.
if ! "$security" add-trusted-cert -d -r trustRoot -p ssl \
    -k /Library/Keychains/System.keychain "$root"; then
    echo "manage.nix: failed to trust the local HTTPS root in the macOS admin trust store" >&2
    exit 1
fi
if ! "$security" verify-cert -q -l -p ssl -c "$root"; then
    echo "manage.nix: local HTTPS root is still untrusted after installation" >&2
    exit 1
fi
