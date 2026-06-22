#!/bin/sh
# Post-launch user command (runs in background; see "Background" in scripts.json).
# Skipped when unset.
[ -n "$RITZ_VAR_post_command" ] || exit 0
cd "$HOME" 2>/dev/null || true
exec sh -c "$RITZ_VAR_post_command $RITZ_VAR_post_args"
