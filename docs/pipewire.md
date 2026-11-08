# PipeWire window inputs (Linux)

This fork adds independent live window inputs to LPMT 2. Each selected window
becomes a media asset that can be assigned to one or more layers and surfaces.
The desktop's ScreenCast portal handles window selection and permission;
native PipeWire streams receive the frames over the portal's restricted remote.

## Build

On Debian/Ubuntu, install these in addition to LPMT's normal windowing dependencies:

```sh
sudo apt install libpipewire-0.3-dev libclang-dev pkg-config
cargo run --release -p lpmt-app --features pipewire --locked
```

PipeWire 0.3.65 or newer, `xdg-desktop-portal` and a desktop portal backend
supporting **window** capture must be running in the same graphical user session. GNOME and KDE provide
window selection; some compositor backends only provide monitor capture, which
is not sufficient for this feature. No FFmpeg or GStreamer feature is required.
The optional dependencies are Linux-only, so default builds and other platforms still build.

## Use

1. In **Mapping → Media shelf**, click **Add windows…**.
2. Select the windows in the desktop sharing dialog. If the desktop allows only
   one window at a time, repeat **Add windows…** to create additional sessions.
3. Select each new **Window N** card. Use **Use for [layer]** or **New layer on
   [surface]** to map it. Each input has its own thumbnail, texture and dimensions.
4. Use the existing warp, mask, crop, fit, chroma key and output controls.

The [two-window example](../assets/pipewire-two-windows.json) already has two input
slots mapped to separate surfaces. Run it with:

```sh
cargo run --release -p lpmt-app --features pipewire --locked -- --project assets/pipewire-two-windows.json
```

Select each card and click **Select window…**. Outputs start closed; open them
from Calibration when ready.

**Disconnect** releases that window input. **Select window…** reconnects its
existing slot, preserving layer assignments. **Cancel selection** dismisses an
outstanding selection without interrupting other inputs.

The show clock does not pause or seek live windows. A paused show still receives
new frames. Window resizing renegotiates dimensions and replaces the texture as
needed. Rendering and all output windows share one texture per live input.
Reloading file media does not restart screen sharing.

Projects save input slots and layer assignments, never ephemeral PipeWire node
IDs, file descriptors or portal permissions. After reopening a project, choose
**Select window…** for each saved input. Disconnected or revoked streams render
black and report their state in the media shelf. Selection cancellation and
connection errors leave the editor responsive. Removing the last input belonging
to a session, opening another project or exiting closes the portal session.

## Implementation limits

PipeWire MemFd/MemPtr buffers are borrowed directly by the GPU uploader on the
capture worker. It preserves row pitch and adjusts the start address for the
visible crop: there is no intermediate pixel `Vec`, CPU crop copy or CPU channel
swizzle. Native BGRA/RGBA textures handle channel order; RGBx/BGRx use a GPU pass
to set alpha to one. Real alpha is preserved. Alpha-bearing formats are preferred
to avoid that pass when the producer supports them. Stable dimensions/formats
reuse textures, views, bind groups and thumbnail registrations across frames.

`Queue::write_texture` copies into wgpu-owned staging memory before returning the
PipeWire buffer. This is **not DMA-BUF zero-copy**: the compositor can still need
a readback, and CPU-to-GPU transfer remains. DMA-BUF import also needs modifier
negotiation, external synchronization and correct buffer lifetime/ownership; it
is not enabled by simply advertising `SPA_DATA_DmaBuf`. See
[PipeWire DMA-BUF sharing](https://docs.pipewire.org/devel/page_dma_buf.html).

Each stream drains to the newest available buffer. At most one unconsumed upload
per input is allowed, so a stalled editor does not accumulate transfers; it
resumes on new producer frames after consuming the pending upload. The PipeWire
loop wakes on file-descriptor readiness instead of an 8 ms polling timer. Streams
in one selection share a portal remote, with independent dimensions and textures.
Per-frame crop metadata also handles compositors that allocate an oversized
buffer and change its visible rectangle when a window resizes.
When provided by the portal, the stable PipeWire serial is used
for targeting; older portals use their session-scoped node ID. Audio capture and
automatic permission restoration are not included.

## Verification

```sh
cargo test --workspace --features lpmt-app/pipewire --locked
cargo clippy --workspace --all-targets --features lpmt-app/pipewire --locked -- -D warnings
```

Manual graphical-session checks:

- Add two distinct animated windows; assign them to two surfaces and verify both
  preview and output update independently, including while transport is paused.
- Resize a source window, reload file media and undo/redo a layer assignment.
- Cancel a new selection while existing inputs continue streaming.
- Disconnect and reconnect one slot while the other continues streaming.
- Close a source window or revoke sharing in the desktop, then reconnect it.
- Save and reopen; confirm slots persist and ask for window selection again.

References: [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html),
[ashpd](https://docs.rs/ashpd/0.14.0/ashpd/desktop/screencast/index.html).

### Checked on 2026-11-08

Ubuntu 24.04 / GNOME 46 / PipeWire 1.0.5, in an isolated X11 desktop on the
Linux development host: selected two animated test windows in one portal session,
mapped them to separate surfaces, verified preview and output rendering, resized
one source (508×336 to 668×426 including decorations), reloaded media, disconnected
and reconnected one input while the other continued, cancelled a new selection,
and saved/reopened the show. Reopening preserved both assignments, closed the
portal sessions and displayed disconnected inputs in black. Physical multi-monitor
presentation and a native Wayland desktop were not exercised in this session.

## Performance verification

Use the release binary for live shows. A reproducible GPU correctness check and
release-to-release upload benchmark are included:

```sh
cargo run --release -p lpmt-render --example capture-bench --locked -- --check-only
cargo run --release -p lpmt-render --example capture-bench --locked
```

The check reads pixels back from the GPU to verify all four formats, alpha,
offset/padded crops, texture reuse and resizing. The benchmark compares the old
allocation/crop-copy/CPU-swizzle/upload path with the direct upload/GPU-alpha path
on the same device, using two BGRx inputs per sample. It uses 20 warmup samples,
120 measured samples and three rounds with alternating A/B order. Each sample
waits for GPU completion; CPU times exclude that wait. These are upload-path
measurements, **not desktop capture-to-display latency or application FPS**.

The optimized release was also exercised on the same isolated GNOME desktop:
two animated window inputs, live resize, per-input disconnect, and a separate
output window. The test application used the RTX 4060 Ti/Vulkan adapter; the
virtual X11 compositor/display is not a representative presentation benchmark.
Linux workspace tests (164) and macOS default tests (161), plus Clippy with
warnings denied, passed. Native Wayland/DMA-BUF and physical multi-monitor timing
remain unmeasured.

### Upload measurements on the development host (2026-11-08)

Ryzen 5 5600X, RTX 4060 Ti, Vulkan, NVIDIA 610.57.04, Ubuntu 24.04,
Rust 1.95, release profile.
The temporary graphical test session was stopped before the final run. GPU clocks
were left under the driver's automatic power management (memory clock observed
at 810 MHz during the 4K run); no power/clock settings were changed. Results vary
with power state and other work on the host. Values below are medians of the
three per-round medians; each sample contains **two** window uploads.

| Inputs | Old CPU time | New CPU time | Old through GPU completion | New through GPU completion |
| --- | ---: | ---: | ---: | ---: |
| 2 × 1920×1080 | 7.609 ms | 1.522 ms | 7.849 ms | 1.773 ms |
| 2 × 3840×2160 | 51.211 ms | 27.759 ms | 52.407 ms | 30.139 ms |

[All rounds, including CPU p95](benchmarks/pipewire-rtx4060ti-2026-11-08.csv).
Per-round CPU medians ranged from 3.839–7.617 ms (old) versus 1.508–1.530 ms
(new) at 1080p, and 40.564–51.360 ms versus 27.723–27.777 ms at 4K. This does not
establish a 4K60 presentation guarantee. The remaining host-to-device transfer
is still material; DMA-BUF support would need separate implementation and
native compositor validation.
