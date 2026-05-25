#!/usr/bin/env bash
# Close beads tickets that have already shipped in code but are still open in
# `bd`. The list below is curated from `git log --grep=rjr` on main; each group
# names the PR that landed the work so reviewers can spot-check.
#
# This script is intentionally dry-run by default. Re-run with --yes after
# eyeballing the printed list to actually invoke `bd close`.
#
#   scripts/close-drifted-tickets.sh           # prints what it would close
#   scripts/close-drifted-tickets.sh --yes     # actually closes them
#
# Tickets that are still in flight (rjr.104, rjr.105 are post-#85 validation
# work) are deliberately omitted; do not add them without re-checking status.

set -u

YES=0
for arg in "$@"; do
  case "$arg" in
    --yes) YES=1 ;;
    -h|--help)
      sed -n '2,15p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

if ! command -v bd >/dev/null 2>&1; then
  echo "bd is not on PATH; install beads first." >&2
  exit 1
fi

# Groups are: <pr-tag> <ticket-id> [...]. Newlines separate groups.
groups=(
  # PR #67 — MCP runtime follow-ups
  "PR#67 rjr.7 rjr.8 rjr.9 rjr.10 rjr.11 rjr.12"
  # PR #78 — F01 tool-call cost / observability
  "PR#78 rjr.1 rjr.2 rjr.3 rjr.4 rjr.5 rjr.6"
  # PR #73 — F05 shell parsing/exec hardening
  "PR#73 rjr.28"
  # PR #79 — F06 TUI follow-ups (.35-.42 inclusive)
  "PR#79 rjr.35 rjr.36 rjr.37 rjr.38 rjr.39 rjr.40 rjr.41 rjr.42"
  # PR #80 — F07 UX & workflow improvements
  "PR#80 rjr.43 rjr.44 rjr.45 rjr.46 rjr.47 rjr.48"
  # PR #72 — F08 async/background work boundaries
  "PR#72 rjr.49 rjr.50 rjr.51 rjr.52 rjr.53 rjr.54"
)

total=0
declare -a closed=()
declare -a failed=()

for entry in "${groups[@]}"; do
  pr="${entry%% *}"
  ids="${entry#* }"
  echo "== $pr =="
  for id in $ids; do
    full="squeezy-$id"
    total=$((total + 1))
    if [[ "$YES" -eq 1 ]]; then
      if bd close "$full" 2>&1; then
        closed+=("$full")
      else
        failed+=("$full")
      fi
    else
      echo "  would close: $full"
    fi
  done
done

if [[ "$YES" -eq 0 ]]; then
  echo
  echo "Dry run: $total tickets would be closed. Re-run with --yes to execute."
  exit 0
fi

echo
echo "Closed: ${#closed[@]} / $total"
if [[ "${#failed[@]}" -gt 0 ]]; then
  echo "Failed (${#failed[@]}):"
  for id in "${failed[@]}"; do
    echo "  $id"
  done
  exit 1
fi
