#!/usr/bin/env bash
# Test a deploy offline by shadowing curl with a mock function.
set -euo pipefail

deploy() {
    local host="$1" resp
    resp="$(curl -s -X POST "https://$host/api/deploy")"
    if [ "$(jq -r .status <<<"$resp")" = ok ]; then
        echo "deployed to $host (rev $(jq -r .rev <<<"$resp"))"
    else
        echo "deploy to $host rejected" >&2; return 1
    fi
}

# Mock: shadow curl for the test. This override is global from here on.
curl() { echo '{"status":"ok","rev":"abc123"}'; }

deploy prod.example.com
deploy staging.example.com

unset -f curl   # easy to forget — until then, every curl in the script is mocked
