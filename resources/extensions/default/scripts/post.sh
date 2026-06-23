#!/bin/sh
# Post-launch user commands. One per multi_string entry; skipped when unset.
# Each runs in parallel (backgrounded) so they don't block one another. The whole
# script is itself spawned in the background (see "Background" in scripts.json).
[ -n "$RITZ_VAR_post_command" ] || exit 0
cd "$HOME" 2>/dev/null || true
set -f                 # don't glob-expand the unquoted command list
IFS='
'
for cmd in $RITZ_VAR_post_command; do
  [ -n "$cmd" ] && sh -c "$cmd" &
done
wait                   # keep this script alive until all parallel commands finish
