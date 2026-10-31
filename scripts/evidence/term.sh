#!/usr/bin/env bash
# Capture a terminal screenshot for an issue's Evidence comment (HANDOVER.md §6).
#
#   scripts/evidence/term.sh <issue-number> <name> <command...>
#
# Runs <command> (through bash -c, with colors forced), echoes the command as a prompt line,
# and renders the combined output with `freeze` to docs/evidence/<issue>/<name>.png.
# Output text is also saved next to it as <name>.txt for reviewers.
set -uo pipefail
if [ $# -lt 3 ]; then
  echo "usage: $0 <issue-number> <name> <command...>" >&2
  exit 2
fi
issue=$1; name=$2; shift 2
cmd="$*"
root=$(git rev-parse --show-toplevel)
out="$root/docs/evidence/$issue"
mkdir -p "$out"
export CARGO_TERM_COLOR=always CLICOLOR_FORCE=1 FORCE_COLOR=1
txt="$out/$name.txt"
{
  printf '\033[1;35m$\033[0m %s\n' "$cmd"
  bash -c "$cmd" 2>&1
} > "$txt"
status=${PIPESTATUS[0]:-$?}
# never leak local home paths into evidence
sed -i.bak -e "s#$root#.#g" -e "s#$HOME#~#g" "$txt" && rm -f "$txt.bak"
command -v freeze >/dev/null || { echo "freeze not found (https://github.com/charmbracelet/freeze)" >&2; exit 1; }
freeze --language ansi --window --padding 20 --margin 0 \
  --background "#15121A" --font.size 13 --line-height 1.3 \
  --output "$out/$name.png" < "$txt" >/dev/null
# strip ANSI codes in the saved text copy
sed -i.bak -E 's/\x1B\[[0-9;?]*[ -\/]*[@-~]//g' "$txt" && rm -f "$txt.bak"
echo "wrote docs/evidence/$issue/$name.png (command exit $status)"
