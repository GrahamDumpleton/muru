#!/bin/sh
# Reject the em dash character (U+2014) in tracked text files, as AGENTS.md requires.

set -eu

cd "$(dirname "$0")/.."

em_dash=$(printf '\342\200\224')
found=0

for file in $(git ls-files); do
    [ -f "$file" ] || continue
    case "$file" in
        scratch/*) continue ;;
    esac
    if grep -I -n "$em_dash" "$file" >/dev/null 2>&1; then
        grep -I -n "$em_dash" "$file" | sed "s|^|$file:|"
        found=1
    fi
done

if [ "$found" -ne 0 ]; then
    echo "em dashes found; rephrase with commas, parentheses, colons or separate sentences" >&2
    exit 1
fi
