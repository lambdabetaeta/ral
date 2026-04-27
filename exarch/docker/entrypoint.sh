#!/bin/sh
# Refuse to start unless at least one provider key is in the env.
# `exarch` itself only complains about the *one* key the chosen provider
# needs, which is unhelpful when none are set: the user gets a confusing
# message about whichever provider happens to be the default.

set -eu

if [ -z "${ANTHROPIC_API_KEY:-}${OPENAI_API_KEY:-}${OPENROUTER_API_KEY:-}" ]; then
    cat >&2 <<'EOF'
exarch-box: refusing to start — no provider key in env.

  Set at least one of:
    ANTHROPIC_API_KEY
    OPENAI_API_KEY
    OPENROUTER_API_KEY

  Pass it on the host before `docker compose run`, e.g. via your shell
  rc, a .env next to compose.yaml, or `--env ANTHROPIC_API_KEY=...`.
EOF
    exit 1
fi

exec /usr/local/bin/exarch "$@"
