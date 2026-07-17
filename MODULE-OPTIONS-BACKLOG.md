# Module Options Backlog (pending sign-off)

Research done 2026-07-17. Candidate options to ADD to existing modules. Nothing
written yet — each row awaits sign-off. ⭐ = recommended (High-value). All verified
against current upstream (source/README/docs) at research time.

## DXVK
- ⭐ `dxgi.syncInterval` / `d3d9.presentInterval` — Off/On/Game — force vsync per-game — High
- ⭐ `dxvk.latencySleep` — Auto/True/False — Reflex/low-latency — High
- `dxgi.hideNvidiaGpu` / `hideAmdGpu` — Auto/T/F — vendor spoof (fixes vendor-detect crashes) — High
- `DXVK_LOG_LEVEL` / `DXVK_LOG_PATH` — none…debug — silence log spam — Med
- `dxvk.maxMemoryBudget` — MB — VRAM cap for low-VRAM GPUs — Med
- (config keys slot into existing DXVK_CONFIG append builder; LOG vars are new top-level ENV_VARS)

## VKD3D
- ⭐ `VKD3D_VULKAN_DEVICE` — int index — GPU select (hybrid/multi-GPU) — High
- ⭐ `VKD3D_FILTER_DEVICE_NAME` — substring — GPU select by name (stable across driver updates) — High
- `VKD3D_SHADER_CACHE_PATH` — path / 0 — relocate/disable cache — Med
- `force_host_cached`, `small_vram_rebar` — toggles — promote from raw_config passthrough — Med

## Proton
- ⭐ `WINE_FULLSCREEN_FSR` (+ `_STRENGTH` 0–5, `_CUSTOM_MODE` WxH) — FSR upscaling — High (GE-only, no-op on stock Valve)
- ⭐ `PROTON_ENABLE_NVAPI` — 1 — DLSS/Reflex on NVIDIA — High (GE-only)
- `PROTON_NO_D3D12` — 1 — force DX11 fallback — Med
- CONFLICT: `PROTON_NO_ESYNC` was suggested for re-add but we removed it (obsoleted in Proton 11,
  verified vs P11 launcher script; GE README still lists it). Leaning: leave out unless targeting GE.

## Gamescope
- ⭐ `--mangoapp` — toggle — built-in perf overlay — High
- ⭐ `--hdr-enabled` — toggle — HDR output — High
- ⭐ `-b`/`--borderless` — toggle — borderless window — High
- `--filter nis` — fold FSR toggle into filter dropdown (Off/Linear/Nearest/FSR/NIS/Pixel) — NIS upscaler — High
- `--rt` — realtime sched — Med
- `--framerate-limit` — ⚠️ worker claimed DIVISOR of refresh, not absolute FPS — VERIFY before wiring — Med
- `--nested-unfocused-refresh` — Hz — battery/fps cap when unfocused — Med
- `--prefer-vk-device` — vendor:device — pin compositor GPU (multi-GPU) — Med

## AMD (RADV)
- ⭐ `RADV_TEX_ANISO` — off/2/4/8/16 — force anisotropic filtering — High
- ⭐ `RADV_PERFTEST=pswave32` / `gewave32` (+ `cswave32`) — append flags — wave32 perf — High/Med
- `RADV_DEBUG=zerovram` — toggle — fix flicker/glitch games (new RADV_DEBUG env entry needed) — Med
- `MESA_VK_WSI_PRESENT_MODE` — fifo/mailbox/immediate/relaxed — force present mode (tearing caveat) — Med
- `mesa_glthread=true` — toggle — OpenGL-game FPS (ROUTED HERE, not misc) — Med

## Misc
- ⭐ MangoHud `mangohud` wrapper (+ `MANGOHUD_CONFIG` string) — perf overlay, GL+Vulkan — High
- `ENABLE_VKBASALT` (+ `VKBASALT_CONFIG_FILE`) — CAS/SMAA post-processing — Med
- `WINEDEBUG=-all` — silence Wine logs (maybe belongs in proton) — Med
- `OBS_VKCAPTURE`, `SDL_JOYSTICK_HIDAPI` — streaming / controller fix — Niche

## Pulse
- ⭐ `PIPEWIRE_LATENCY` — e.g. `256/48000` — the knob that ACTUALLY works on PipeWire (PULSE_LATENCY_MSEC
  only hits pulse-compat path); pair the two in one Latency section — High
- ⭐ `SDL_AUDIODRIVER` — pipewire/pulseaudio/alsa — fix wrong/no-audio backend — High
- `ALSOFT_DRIVERS` — backend list — OpenAL backend fix — Med
- `PULSE_SOURCE` — mic name — symmetry with PULSE_SINK — Med

## Cross-module decisions
1. MangoHud appears twice — gamescope `--mangoapp` vs misc `mangohud` wrapper. Pick ONE
   (wrapper works without gamescope; --mangoapp only when gamescope active).
2. `PROTON_NO_ESYNC` re-add — see Proton conflict note above.
3. gamescope `--framerate-limit` divisor-vs-absolute — verify before wiring.

Minimal high-value set = the ⭐ rows only (~14 options).
