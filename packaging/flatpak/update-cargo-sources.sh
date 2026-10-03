#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

GENERATOR_COMMIT='41c20aa10819cdb2a4f3ca171758a96d1955c018'
GENERATOR_URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$GENERATOR_COMMIT/cargo/flatpak-cargo-generator.py"
GENERATOR_SHA256='0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380'
OUT="${OUT:-$here/cargo-sources.json}"

fail() { echo "ERROR: $*" >&2; exit 1; }
for tool in git curl sha256sum uv python3; do
    command -v "$tool" >/dev/null || fail "$tool not found (install it and re-run)"
done

mapfile -d '' -t lockfiles < <(git -C "$repo" ls-files -z -- ':(glob)**/Cargo.lock')
[ "${#lockfiles[@]}" -gt 0 ] || fail "git tracks no Cargo.lock in $repo"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

generator="$work/flatpak-cargo-generator.py"
curl --fail --location --silent --show-error --output "$generator" "$GENERATOR_URL" \
    || fail "could not download flatpak-cargo-generator.py from $GENERATOR_URL"
echo "$GENERATOR_SHA256  $generator" | sha256sum --check --quiet - \
    || fail "flatpak-cargo-generator.py does not match the pinned sha256 $GENERATOR_SHA256"

parts=()
for lockfile in "${lockfiles[@]}"; do
    [ -f "$repo/$lockfile" ] || fail "lockfile missing: $repo/$lockfile"
    part="$work/${lockfile//\//_}.json"
    uv run --quiet --script "$generator" "$repo/$lockfile" --output "$part"
    parts+=("$part")
done

python3 - "$OUT" "${parts[@]}" <<'MERGE'
import json
import sys

out, *parts = sys.argv[1:]
merged = []
seen = set()
for part in parts:
    with open(part, encoding="utf-8") as handle:
        for source in json.load(handle):
            key = json.dumps(source, sort_keys=True)
            if key not in seen:
                seen.add(key)
                merged.append(source)

configs = [source for source in merged if source.get("dest-filename") == "config"]
if len(configs) != 1:
    sys.exit(f"the lockfiles need {len(configs)} different cargo source configs; expected one")
merged.remove(configs[0])
merged.append({**configs[0], "dest-filename": "config.toml"})

with open(out, "w", encoding="utf-8") as handle:
    json.dump(merged, handle, indent=4)
    handle.write("\n")
print(f"wrote {len(merged)} sources to {out}")
MERGE
