#!/usr/bin/env bash
# Run some operations inside a best-effort sandbox.
set -euo pipefail

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

echo 'outside the sandbox:'
echo ok > "$sandbox/scratch" && echo '  allowed  write inside /tmp'

echo
echo 'inside a bubblewrap sandbox (rw only the scratch dir, no net, no exec):'
bwrap \
    --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
    --bind "$sandbox" "$sandbox" \
    --unshare-all --die-with-parent \
    /bin/sh -c '
        echo ok > "'"$sandbox"'/inside" && echo "  allowed  write inside sandbox"
        echo no > /tmp/escape           && echo "  allowed  write outside sandbox"
        cat /etc/hosts >/dev/null 2>&1  && echo "  allowed  read /etc/hosts"
        curl http://example.com 2>/dev/null && echo "  allowed  exec curl"
    '
