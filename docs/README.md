# Ritz — Documentation

Ritz is a Linux game launcher with a pluggable extension system. Start with
[architecture/overview.md](architecture/overview.md) for the big picture, then use the
sections below to find the doc you need.

## architecture/ — how the code fits together

- [Overview](architecture/overview.md) — big-picture hub: module map, data flow, where to look for X. **Start here.**

## features/ — what the launcher does

- [Extension System](features/extension-system.md) — the module/builder engine extensions plug into.
- [Scoped Config](features/scoped-config.md) — four-layer config inheritance (global/desktop/module/game).
- [Settings GUI](features/settings-gui.md) — the settings window and how it renders module config.
- [Splash & New-Game Wizard](features/splash-and-new-game-wizard.md) — launch splash screen and new-game flow.
- [Process Supervisor](features/process-supervisor.md) — game process lifecycle management.
- [Hooks & Scripts](features/hooks-and-scripts.md) — lifecycle hooks and user scripts.
- [Launch Command Assembly](features/launch-command-assembly.md) — `%command%` and launch block assembly.
- [Bundled Modules](features/bundled-modules.md) — reference for modules shipped with Ritz.
- [Runtime Backends](features/runtime-backends.md) — the LSFG-VK and Hypr-Monctl `Backend`-trait handlers (the list-backed `custom-env`/`custom-game-env`/`custom-args` `Backend` values are a builder pre-pass, documented in Launch Command Assembly instead).
- [Resource Export](features/resource-export.md) — exporting embedded resources (e.g. bundled plugin binaries).

## ui/ — interface conventions

- [Styling Guide](ui/STYLING-GUIDE.md) — egui styling conventions and reusable helpers.

## brainstorm/ — design plans & decision logs

- [Custom Module Editor](brainstorm/custom-module-editor.md) — design plan and decision log for the GUI custom-module editor feature (branch `feat/custom-module-editor`).
- [IDE Mode](brainstorm/ide-mode.md) — design plan for the three-column module authoring mode (module tree + editor + live WYSIWYG preview), with staged rollout S1–S6.

## claude-instructions/ — mandatory rules for agents

- [Working with the Docs](claude-instructions/documentation.md) — doc-maintenance discipline: read before you touch, update after you change.
- [Documentation Version Policy](claude-instructions/documentation-version-policy.md) — how doc/version markers get bumped.

## meta/

- [Terminology](meta/TERMINOLOGY.md) — project-specific vocabulary glossary.
