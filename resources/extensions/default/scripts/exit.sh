#!/bin/sh
# Post-exit user command (blocks until done). Skipped when unset.
[ -n "$RITZ_VAR_exit_command" ] || exit 0
cd "$HOME" 2>/dev/null || true
exec sh -c "$RITZ_VAR_exit_command $RITZ_VAR_exit_args"
