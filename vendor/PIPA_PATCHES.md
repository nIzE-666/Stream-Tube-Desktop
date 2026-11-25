# Xiaomi Pad 6 (`pipa`) Vendored Crate Patches

These three vendored crates are patched via `[patch.crates-io]` for hardware Adreno 650
OpenGL ES 3.2 (libhybris / Halium 13) and Wayland touch scrolling on `naomi-shell` (Ubuntu Touch 26.04).
All behavior is opt-in via environment variables or guarded to OpenGL ES, leaving desktop Linux/Windows/macOS defaults intact.

## `vendor/gpui-pre-linux` (0.3.12)
- `src/linux/wayland/window.rs`:
  - `GPUI_WAYLAND_PIXEL_SCALE`: honor fractional/fixed HiDPI scale (`2.0` on `pipa`).
  - `GPUI_WAYLAND_NO_FRAME_CALLBACK`: optional non-blocking draw mode (`0` uses native 120 Hz Wayland frame callbacks).
  - `GPUI_WAYLAND_SERVER_DECORATIONS`: respect compositor server-side decorations (`naomi-shell` titlebar) and start the surface opaque (`transparent: !force_server_decorations()`) to avoid compiling all 9 GPU pipelines twice at startup.
  - Removed the `attach(None)` unmap hack on resize and fixed unmaximize window dimensions so maximize/unmaximize never leaves a black surface.
- `src/linux/wayland/client.rs`:
  - `GPUI_WAYLAND_SCROLL_MULTIPLIER`: scale Wayland axis scroll deltas (`3.4` compensates for `naomi-shell` `touchScrollSpeed=35%`).
  - Suppress stray pointer motion events for 90 ms during active touch scroll to avoid accidental hover jitter.

## `vendor/gpui-pre-wgpu` (0.3.12)
- `src/wgpu_context.rs`:
  - `WGPU_BACKEND=gl|vulkan`: allow selecting native OpenGL ES 3.2 (`gl`) to bypass Halium Vulkan swapchain resize deadlocks.
  - GLES-compatible `required_limits` (`downlevel_webgl2_defaults` + adapter storage buffer limits).
  - Use `Rgba8Unorm` color atlas on native `wgpu::Backend::Gl` (matches GLES texture formats without swizzle penalty).
  - `STREAMTUBE_WEBGL_INSTANCES=1`: fetch GPUI quad/sprite instance data via `Rgba32Uint` texture (`texelFetch`, L1 texture cache) instead of per-pixel SSBO reads in `fs_quad` (fixes 258 ms Adreno 650 GLES SSBO bottleneck: ~4 fps -> 75-92 fps).
  - `STREAMTUBE_NO_DUAL_BLEND`: optional diagnostic toggle.
- `src/wgpu_renderer.rs`:
  - Prefer `Rgba8Unorm` before `Bgra8Unorm` on `wgpu::Backend::Gl` surfaces.

## `vendor/wgpu-hal` (30.0.2)
- `src/gles/adapter.rs`:
  - Disable `BUFFER_ALLOCATION` (`GL_EXT_buffer_storage` no-op stub under libhybris causes `glMapBufferRange` -> NULL / `DeviceError::Lost`) and `MULTISAMPLED_RENDER_TO_TEXTURE` when running on OpenGL ES (`es_ver.is_some()`).
- `src/gles/egl.rs`:
  - Fall back to `&[ATTRIB_NONE]` in `create_platform_window_surface` when extended attributes are rejected by Android/Halium EGL.
  - Set `eglSwapInterval(0)` when `GPUI_WAYLAND_NO_FRAME_CALLBACK=1`.

## Rebasing onto a new upstream Stream Tube release
1. `git fetch origin --tags`
2. If upstream kept `gpui-pre-*` 0.3.12 and `wgpu-hal` 30.0.2: `git rebase v0.7.x-alpha` directly.
3. If upstream bumped `gpui-pre-*` or `wgpu-hal`: re-vendor the new crate version and re-apply the ~8 small hunks listed above.
4. Run `.\tools\pipa-sync-build.ps1` to sync, build in `devroot`, strip, and atomically install on `pipa`.
