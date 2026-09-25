#!/usr/bin/env bash
# Print a repository's default branch: origin's HEAD, then git's configured default, then main.

repo="$1"

cd -- "$repo" || exit 1

git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's@^origin/@@' \
    || git config --get init.defaultBranch \
    || echo main
