#!/usr/bin/env bash
# install.sh — single entry point for install / update / remove of ritz,
# from a fresh clone. Self-contained: ritz is one cargo binary with its
# resources/ baked in at compile time (include_dir!), so there is nothing to
# install besides the binary itself.
#
# Installs to /usr/bin/ritz. Never touches your settings (~/.config/ritz).
#
# Usage:
#   ./install.sh [--install|--remove|--update] [options]
#
# Actions (choose at most one; no action = interactive menu, Enter defaults
# to Install/reinstall):
#   --install           check tools, build a release binary if needed, install
#                       it (symlink or copy — asked interactively unless
#                       --link/--copy is given; Enter defaults to symlink).
#   --remove            uninstall the binary/symlink this installed. Never
#                       touches ~/.config/ritz.
#   --update            git pull --ff-only (a dirty tree only warns; the pull
#                       itself refuses anything unsafe), rebuild, and
#                       reinstall by whichever method (symlink/copy) is
#                       already in place — a symlink install needs no copy
#                       step, the rebuilt binary is live immediately.
#   -h, --help          show this help and exit
#
# Options:
#   --link              symlink mode for --install, non-interactive
#                       (target -> the built binary; a later --update just
#                       rebuilds and the installed name is live immediately,
#                       no root needed again — but moving/deleting this repo
#                       breaks it)
#   --copy              copy mode for --install, non-interactive (target is
#                       independent of this repo; --update copies again)
#   --yes, -y           assume "yes" to all confirmation prompts
#   --rebuild           (--install) rebuild even if a release binary exists
#   --force             force a full recompile: runs `cargo clean --release`
#                       first, so everything is rebuilt even when nothing
#                       changed.
#   --prefix DIR        install directory (default: /usr/bin) — override to
#                       install/remove/update into a scratch prefix, e.g. for
#                       testing this script without touching the real system
#
# Honors CARGO_TARGET_DIR if set (default: <repo>/target).
#
# Examples:
#   ./install.sh                          # detect state, offer a menu
#   ./install.sh --install                # ask link/copy, build if needed
#   ./install.sh --install --link --yes   # scripted symlink install
#   ./install.sh --update                 # pull, rebuild, reinstall in place
#   ./install.sh --remove --yes

set -euo pipefail

BIN_NAME="ritz"
REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)

err() { printf 'error: %s\n' "$*" >&2; }
info() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }

# --- basic build-tool check ---------------------------------------------
# Checked first, before argument parsing, so a missing tool fails fast with
# one clear message instead of a confusing error halfway through.
check_build_tools() {
	local missing=() cmd
	for cmd in cargo git; do
		command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
	done
	[ "${#missing[@]}" = "0" ] && return 0

	# Package names: cargo ships as `rust` on Arch, `cargo` elsewhere.
	err "missing required command(s): ${missing[*]}"
	if command -v pacman >/dev/null 2>&1; then
		err "install them with: sudo pacman -S --needed $(printf '%s\n' "${missing[@]}" | sed 's/^cargo$/rust/' | tr '\n' ' ')"
	elif command -v dnf >/dev/null 2>&1; then
		err "install them with: sudo dnf install ${missing[*]}"
	elif command -v apt-get >/dev/null 2>&1; then
		err "install them with: sudo apt install ${missing[*]}"
	else
		err "install them through your distro's package manager, or Rust via https://rustup.rs/"
	fi
	exit 1
}
check_build_tools

# Kept so do_update() can re-exec this script verbatim after a pull that
# changed it.
ORIGINAL_ARGS=("$@")

ACTION=""           # "install", "remove" or "update"; "" = interactive menu
MODE=""             # "link" or "copy" (--install only)
ASSUME_YES=0
REBUILD=0
FORCE_CLEAN=0
PREFIX_DIR="/usr/bin"

# The whole header comment above, minus the shebang.
print_help() { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'; }

set_action() {
	if [ -n "$ACTION" ] && [ "$ACTION" != "$1" ]; then
		err "choose at most one of --install / --remove / --update (got --$ACTION and --$1)."
		exit 1
	fi
	ACTION="$1"
}

while [ $# -gt 0 ]; do
	case "$1" in
		--install) set_action "install" ;;
		--remove|--uninstall) set_action "remove" ;;
		--update) set_action "update" ;;
		--link) MODE="link" ;;
		--copy) MODE="copy" ;;
		--yes|-y) ASSUME_YES=1 ;;
		--rebuild) REBUILD=1 ;;
		--force) FORCE_CLEAN=1 ;;
		--prefix) [ $# -ge 2 ] || { err "--prefix needs a directory"; exit 1; }; PREFIX_DIR="$2"; shift ;;
		-h|--help) print_help; exit 0 ;;
		*) err "unknown option: $1"; print_help; exit 1 ;;
	esac
	shift
done

PREFIX_DIR=$(realpath -m -- "$PREFIX_DIR")
TARGET="$PREFIX_DIR/$BIN_NAME"
RELEASE_BIN="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/$BIN_NAME"
RELEASE_BIN=$(realpath -m -- "$RELEASE_BIN")

# --- helpers -------------------------------------------------------------

# Run a command with sudo only if $PREFIX_DIR isn't writable by us.
as_priv() {
	local dir="$PREFIX_DIR"
	while [ ! -e "$dir" ]; do dir=$(dirname -- "$dir"); done  # nearest existing ancestor
	if [ "$(id -u)" = "0" ] || [ -w "$dir" ]; then
		"$@"
	elif command -v sudo >/dev/null 2>&1; then
		sudo "$@"
	else
		err "'$PREFIX_DIR' is not writable by $(id -un) and 'sudo' is not installed."
		err "Re-run this script as root, or install sudo, to write to that path."
		exit 1
	fi
}

confirm() {
	local prompt="$1" default="${2:-n}" reply hint="y/N"
	[ "$ASSUME_YES" = "1" ] && return 0
	if [ ! -t 0 ]; then
		err "not an interactive terminal and no --yes given; refusing to guess on: $prompt"
		exit 1
	fi
	[ "$default" = "y" ] && hint="Y/n"
	read -r -p "$prompt [$hint] " reply || true
	reply="${reply:-$default}"
	case "$reply" in
		[Yy]|[Yy][Ee][Ss]) return 0 ;;
		*) return 1 ;;
	esac
}

target_state() {
	if [ -L "$TARGET" ]; then echo symlink
	elif [ -f "$TARGET" ]; then echo copy
	elif [ -e "$TARGET" ]; then echo other
	else echo absent
	fi
}

# Running ritz processes keep the old binary until they exit — worth saying,
# since ritz stays alive supervising a game for the whole session.
note_if_running() {
	local pids
	pids=$(pgrep -x "$BIN_NAME" 2>/dev/null | tr '\n' ' ' || true)
	[ -n "$pids" ] && info "note: $BIN_NAME is currently running (pid(s): $pids) — it keeps the old binary until restarted."
	return 0
}

build_release() {
	if [ "$(id -u)" = "0" ]; then
		err "refusing to build as root — it would leave root-owned files in the target directory."
		err "Re-run as your normal user; only the install step needs sudo, and this script asks for it only then."
		exit 1
	fi
	if [ "$FORCE_CLEAN" = "1" ]; then
		info "--force: cargo clean --release ..."
		(cd -- "$REPO_ROOT" && cargo clean --release)
	fi
	local nice_cmd=(nice -n 10)
	command -v ionice >/dev/null 2>&1 && nice_cmd=(ionice -c3 "${nice_cmd[@]}")
	info "building (${nice_cmd[*]} cargo build --release --locked)..."
	(cd -- "$REPO_ROOT" && "${nice_cmd[@]}" cargo build --release --locked)
	if [ ! -x "$RELEASE_BIN" ]; then
		err "build finished but $RELEASE_BIN was not produced. Aborting."
		exit 1
	fi
	note_if_running
}

# --- actions -------------------------------------------------------------

do_install() {
	info "ritz installer"
	info "repo:   $REPO_ROOT"
	info "target: $TARGET"

	if [ ! -x "$RELEASE_BIN" ] || [ "$REBUILD" = "1" ] || [ "$FORCE_CLEAN" = "1" ]; then
		if [ ! -x "$RELEASE_BIN" ]; then
			info "no release build found at $RELEASE_BIN — building one now (a few minutes the first time)."
		else
			info "rebuilding $RELEASE_BIN."
		fi
		build_release
	else
		info "found existing release build: $RELEASE_BIN (pass --rebuild to force a rebuild)"
	fi

	if [ -z "$MODE" ]; then
		if [ "$ASSUME_YES" = "1" ]; then
			MODE="copy"
		else
			echo
			echo "How should $TARGET be installed?"
			echo "  1) symlink -> $RELEASE_BIN"
			echo "     Rebuilds go live instantly (a later ./install.sh --update"
			echo "     just rebuilds, no root required again). Breaks if you"
			echo "     move or delete this repo."
			echo "  2) copy (independent of this repo; --update copies again)"
			read -r -p "Choose [1/2, default 1]: " choice
			case "${choice:-1}" in
				1) MODE="link" ;;
				2) MODE="copy" ;;
				*) err "invalid choice: $choice"; exit 1 ;;
			esac
		fi
	fi

	case "$MODE" in
		link) info "about to symlink $TARGET -> $RELEASE_BIN" ;;
		copy) info "about to copy $RELEASE_BIN -> $TARGET" ;;
	esac
	confirm "Proceed writing to $TARGET?" y || { info "aborted, nothing installed."; exit 1; }

	as_priv mkdir -p -- "$PREFIX_DIR"
	case "$MODE" in
		link) as_priv ln -sfn -- "$RELEASE_BIN" "$TARGET" ;;
		copy) as_priv cp -f -- "$RELEASE_BIN" "$TARGET" ;;
	esac
	info "installed: $TARGET ($MODE mode)"
	echo
	info "done. In Steam, set a game's Launch Options to: $BIN_NAME %command%"
	info "run '$BIN_NAME' with no arguments to open the settings GUI."
	[ "$MODE" = "link" ] && warn "symlink mode: moving or deleting $REPO_ROOT will break $TARGET."
	return 0
}

do_remove() {
	info "ritz remover"
	info "target: $TARGET"

	case "$(target_state)" in
		absent)
			info "$TARGET does not exist, nothing to remove."
			info "your settings (~/.config/ritz) are never touched by this script."
			exit 0 ;;
		symlink) info "$TARGET is a symlink -> $(readlink -- "$TARGET")" ;;
		copy) info "$TARGET is a regular file ($(du -h -- "$TARGET" | cut -f1))." ;;
		other) err "$TARGET exists but is neither a symlink nor a regular file. Refusing to touch it."; exit 1 ;;
	esac

	confirm "Remove $TARGET?" n || { info "aborted, nothing removed."; exit 1; }
	as_priv rm -f -- "$TARGET"
	info "removed $TARGET."
	echo
	info "your settings (~/.config/ritz) are kept — this script never touches them."
}

do_update() {
	local install_mode branch head_before head_after

	info "ritz updater"
	info "repo:   $REPO_ROOT"
	info "target: $TARGET"

	case "$(target_state)" in
		absent) err "$TARGET does not exist. Run ./install.sh --install first."; exit 1 ;;
		symlink) install_mode="symlink" ;;
		copy) install_mode="copy" ;;
		*) err "$TARGET exists but is neither a symlink nor a regular file. Refusing to guess."; exit 1 ;;
	esac
	info "detected install mode: $install_mode"

	branch=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)
	info "branch: $branch"

	if [ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]; then
		warn "uncommitted changes in $REPO_ROOT — pulling anyway."
		warn "git pull --ff-only below refuses on its own if the update would clobber anything."
		git -C "$REPO_ROOT" status --short >&2
	fi

	head_before=$(git -C "$REPO_ROOT" rev-parse HEAD)
	info "git pull --ff-only (branch $branch) ..."
	if ! git -C "$REPO_ROOT" pull --ff-only; then
		err "git pull failed (conflict, diverged history, or network error)."
		err "Nothing was built or installed. Resolve the git state by hand and re-run."
		exit 1
	fi
	head_after=$(git -C "$REPO_ROOT" rev-parse HEAD)

	# bash reads scripts lazily, so continuing to run a file the pull just
	# rewrote can execute garbage. Restart once on the new version instead.
	# ponytail: restarts on any new commit, not only ones touching install.sh.
	if [ "$head_before" != "$head_after" ] && [ -z "${RITZ_INSTALL_REEXECED:-}" ]; then
		info "pulled new commits — restarting the installer on the new version."
		export RITZ_INSTALL_REEXECED=1
		exec "$REPO_ROOT/install.sh" "${ORIGINAL_ARGS[@]}"
	fi

	build_release

	if [ "$install_mode" = "symlink" ]; then
		if [ "$(readlink -f -- "$TARGET")" = "$RELEASE_BIN" ]; then
			info "symlink mode: $TARGET already points at the binary just rebuilt, nothing to copy."
		else
			warn "$TARGET points at $(readlink -- "$TARGET"), not this repo's build."
			confirm "Re-point it to $RELEASE_BIN?" y || { info "left $TARGET as-is."; return 0; }
			as_priv ln -sfn -- "$RELEASE_BIN" "$TARGET"
		fi
	else
		info "copy mode: copying $RELEASE_BIN -> $TARGET"
		as_priv cp -f -- "$RELEASE_BIN" "$TARGET"
	fi

	echo
	info "done. $TARGET is up to date."
}

interactive_menu() {
	local state dirty="clean" branch choice
	state=$(target_state)
	branch=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
	[ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ] && dirty="dirty"

	echo
	echo "ritz install.sh — detected state"
	echo "  repo:        $REPO_ROOT (branch $branch, $dirty)"
	echo "  target:      $TARGET"
	case "$state" in
		symlink) echo "  installed:   yes, symlink -> $(readlink -- "$TARGET")" ;;
		copy)    echo "  installed:   yes, copy" ;;
		other)   echo "  installed:   something is there, but not a symlink or regular file" ;;
		absent)  echo "  installed:   no" ;;
	esac
	echo
	echo "What would you like to do?"
	echo "  1) Install$([ "$state" != "absent" ] && echo "/reinstall")"
	echo "  2) Update (git pull + rebuild + reinstall in place)"
	echo "  3) Remove"
	echo "  4) Quit"
	read -r -p "Choose [1-4, default 1]: " choice
	case "${choice:-1}" in
		1) do_install ;;
		2) do_update ;;
		3) do_remove ;;
		4) info "nothing to do." ;;
		*) err "invalid choice: $choice"; exit 1 ;;
	esac
}

case "$ACTION" in
	install) do_install ;;
	remove) do_remove ;;
	update) do_update ;;
	"") interactive_menu ;;
esac
