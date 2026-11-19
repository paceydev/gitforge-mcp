#!/usr/bin/env bash
# Fails if a file git would commit (tracked, or untracked and not ignored)
# contains something that must never be published:
# a credential-shaped string, or a name from PRIVATE_PATTERNS.
#
# PRIVATE_PATTERNS is a newline-separated list of extended regexes (internal
# host names, organisation names) kept outside the repository: in CI it is an
# Actions variable, so the list itself is never published either.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

status=0
credential_shapes='(ghp_|gho_|ghu_|ghs_|github_pat_)[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}|-----BEGIN [A-Z ]*PRIVATE KEY-----'
if git grep --untracked -nIE "$credential_shapes" -- . ':!scripts/check-clean.sh'; then
  echo "credential-shaped strings above" >&2
  status=1
fi

if [ -n "${PRIVATE_PATTERNS:-}" ]; then
  while IFS= read -r pattern; do
    [ -z "$pattern" ] && continue
    if git grep --untracked -nIiE -- "$pattern" >/dev/null; then
      # Show where, but not the pattern: the log may be public one day.
      git grep --untracked -nIiE -l -- "$pattern" | sed 's/^/private name in: /' >&2
      status=1
    fi
  done <<< "$PRIVATE_PATTERNS"
else
  echo "PRIVATE_PATTERNS is not set: checked credential shapes only" >&2
fi
exit $status
