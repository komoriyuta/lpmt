# PipeWire window inputs (Linux)

This fork adds independent live window inputs to LPMT 2. Each selected window
becomes a media asset that can be assigned to one or more layers and surfaces.
The desktop's ScreenCast portal handles window selection and permission;
native PipeWire streams receive the frames over the portal's restricted remote.

## Build

On Debian/Ubuntu, install these in addition to LPMT's normal windowing dependencies:

```sh
sudo apt install libpipewire-0.3-dev libclang-dev pkg-config
cargo run -p lpmt-app --features pipewire --locked
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
cargo run -p lpmt-app --features pipewire --locked -- --project assets/pipewire-two-windows.json
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

Frames currently take a CPU RGBA conversion/upload path, not DMA-BUF zero-copy.
Each stream has a latest-frame mailbox; queued older frames are dropped instead
of building latency. Streams in the same selection share one portal remote and
have separate format negotiation, crop metadata and GPU textures. RGBA, RGBx,
BGRA and BGRx CPU buffers are supported; row stride, buffer offsets and the
per-frame window crop are applied before uploading. This also handles compositors
that allocate a large buffer and change its visible crop when a window resizes.
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
