#!/usr/bin/env bash
# Deploy everything except dev/ from ral-private to the public ral repo.
set -euo pipefail

PRIVATE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PUBLIC_REPO="git@github.com:lambdabetaeta/ral.git"
PUBLIC_DIR="$PRIVATE_ROOT/dev/ral-public"

# Clone public repo if not present
if [ ! -d "$PUBLIC_DIR/.git" ]; then
    echo "Cloning public repo into $PUBLIC_DIR ..."
    git clone "$PUBLIC_REPO" "$PUBLIC_DIR"
fi

echo "Syncing $PRIVATE_ROOT -> $PUBLIC_DIR (excluding dev/, respecting .gitignore) ..."
rsync -av --delete \
    --exclude='.git/' \
    --exclude='dev/' \
    --exclude='CLAUDE.md' \
    --exclude='bench' \
    --exclude='.github/workflows/transfer.yml' \
    --filter=':- .gitignore' \
    "$PRIVATE_ROOT/" "$PUBLIC_DIR/"

# Drop exarch/ from the synced workspace so the public repo builds.
sed -i.bak -E 's/,[[:space:]]*"exarch"//; s/"exarch"[[:space:]]*,[[:space:]]*//' \
    "$PUBLIC_DIR/Cargo.toml" && rm -f "$PUBLIC_DIR/Cargo.toml.bak"

cd "$PUBLIC_DIR"

# --porcelain prints one line per changed/untracked file; empty output means nothing to commit.
# Unlike `git diff`, this catches untracked files (e.g. newly synced files not yet in the index).
if [ -z "$(git status --porcelain)" ]; then
    echo "Nothing changed. Public repo is already up to date."
    exit 0
fi

COMMIT_MSG="${1:-sync from ral-private}"
git add -A
git commit -m "$COMMIT_MSG"
git push

cd "$PRIVATE_ROOT"

# Publish release artifacts to the public repo if they exist.
ARTIFACT_DIR="$PRIVATE_ROOT/target/release-artifacts"
if [ -d "$ARTIFACT_DIR" ] && [ -n "$(ls -A "$ARTIFACT_DIR" 2>/dev/null)" ]; then
    echo "Publishing 'latest' release to public repo ..."
    NOTES="Synced from ral-private on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    FILES=("$ARTIFACT_DIR"/*)

    # Tag the sync commit in the public repo and push it.
    cd "$PUBLIC_DIR"
    git tag -f latest
    git push -f origin refs/tags/latest

    # Recreate the release on the public repo.
    gh release delete latest --repo lambdabetaeta/ral --yes --cleanup-tag=false 2>/dev/null || true
    gh release create latest \
        --repo lambdabetaeta/ral \
        --title 'Latest Build' \
        --prerelease \
        --notes "$NOTES" \
        "${FILES[@]}"
    cd "$PRIVATE_ROOT"
    echo "Release published: https://github.com/lambdabetaeta/ral/releases/tag/latest"
else
    echo "No artifacts in $ARTIFACT_DIR — skipping release publish."
    echo "(Run build-release.ral first if you want a release.)"
fi

rm -rf "$PUBLIC_DIR"

echo "Done. Public repo updated and $PUBLIC_DIR removed."
