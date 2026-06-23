#!/bin/sh
# Post-exit user commands (block until done). One per multi_string entry; skipped when unset.
[ -n "$RITZ_VAR_exit_command" ] || exit 0
cd "$HOME" 2>/dev/null || true
printf '%s\n' "$RITZ_VAR_exit_command" | while IFS= read -r cmd; do
  [ -n "$cmd" ] && sh -c "$cmd"
done
