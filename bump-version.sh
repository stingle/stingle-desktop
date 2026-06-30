#!/usr/bin/env bash
#
# Bump the app version across all version-bearing files, commit, tag, and push.
#
# Usage: ./bump-version.sh 0.1.4
#
# Updates:
#   - app/src-tauri/tauri.conf.json   ("version")
#   - app/src-tauri/Cargo.toml        ([package] version)
#   - app/package.json                ("version")
#   - Cargo.lock                      (the `app` crate entry, via cargo)
#
# Then commits ONLY those files (your other working-tree changes are left
# untouched), tags vX.Y.Z, and pushes the branch + tags. Pushing the tag is
# what triggers the release workflow.

set -euo pipefail

VERSION="${1:-}"

if [[ -z "$VERSION" ]]; then
  echo "error: missing version argument" >&2
  echo "usage: $0 <X.Y.Z>   e.g. $0 0.1.4" >&2
  exit 1
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version must look like X.Y.Z (got '$VERSION')" >&2
  exit 1
fi

# Always operate from the repo root, regardless of where the script is run from.
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

TAG="v$VERSION"

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "error: tag $TAG already exists" >&2
  exit 1
fi

echo "Bumping version to $VERSION ..."

# 1. tauri.conf.json — first "version": "..." (the top-level app version).
sed -i.bak -E '0,/"version": *"[^"]*"/ s//"version": "'"$VERSION"'"/' \
  app/src-tauri/tauri.conf.json

# 2. Cargo.toml — first `version = "..."` (under [package]).
sed -i.bak -E '0,/^version = "[^"]*"/ s//version = "'"$VERSION"'"/' \
  app/src-tauri/Cargo.toml

# 3. package.json — first "version": "...".
sed -i.bak -E '0,/"version": *"[^"]*"/ s//"version": "'"$VERSION"'"/' \
  app/package.json

# Remove sed backup files.
rm -f app/src-tauri/tauri.conf.json.bak app/src-tauri/Cargo.toml.bak app/package.json.bak

# 4. Cargo.lock — patch the `app` crate entry directly (no cargo required).
# Replaces the `version = "..."` line that immediately follows `name = "app"`.
awk -v ver="$VERSION" '
  prev == "name = \"app\"" && /^version = "/ { sub(/"[^"]*"/, "\"" ver "\"") }
  { prev = $0; print }
' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock

# Stage only the files we touched, then commit / tag / push.
git add app/src-tauri/tauri.conf.json app/src-tauri/Cargo.toml app/package.json Cargo.lock

git commit -m "version bump to $VERSION"
git tag "$TAG"

git push
git push --tags

echo "Done: committed, tagged $TAG, and pushed."
