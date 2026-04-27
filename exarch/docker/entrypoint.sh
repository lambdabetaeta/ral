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

# Prepend --provider / --model from env when set; they come before "$@" so
# an explicit CLI flag passed to `docker compose run` is seen later by clap
# and will conflict (clap rejects duplicates).  Don't set both.
[ -n "${EXARCH_PROVIDER:-}" ] && set -- --provider "$EXARCH_PROVIDER" "$@"
[ -n "${EXARCH_MODEL:-}"    ] && set -- --model    "$EXARCH_MODEL"    "$@"

exec /usr/local/bin/exarch "$@"
