#!/bin/sh
# Reject commit messages carrying agent attribution trailers, as AGENTS.md requires.
# Usage: check-commit-message.sh FILE (the commit message file given to a commit-msg hook).

set -eu

file="${1:?commit message file required}"

if grep -i -n -E '^(co-authored-by|signed-off-by: .*(claude|anthropic|copilot|cursor|codex)|generated-by):' "$file" >/dev/null 2>&1 \
    || grep -i -n -E 'generated with \[?claude|🤖' "$file" >/dev/null 2>&1; then
    echo "commit message contains an attribution trailer, which this repository does not use:" >&2
    grep -i -n -E 'co-authored-by|signed-off-by|generated' "$file" >&2 || true
    exit 1
fi
