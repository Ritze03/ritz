#!/bin/sh
# Pre-launch user command (blocks until done). Skipped when unset.
[ -n "$RITZ_VAR_pre_command" ] || exit 0
cd "$HOME" 2>/dev/null || true
exec sh -c "$RITZ_VAR_pre_command $RITZ_VAR_pre_args"
