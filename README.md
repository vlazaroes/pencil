# Pencil

Full-screen drawing overlay for Windows, in pure Rust (only `windows-sys`).

## Install

Download `PencilSetup.exe` from the
[latest release](https://github.com/vlazaroes/pencil/releases/latest) and run it.
It installs per user (no admin rights) into `%LOCALAPPDATA%\Programs\Pencil`, adds a
Start menu entry and, by default, starts Pencil with Windows (can be turned off in
*Settings*). Uninstall from Windows Settings > Apps.

Installed copies check GitHub for a newer release once per start and, if there is
one, download it and update themselves silently (never while you are drawing or
have Settings open).

## Build from source

```
cargo build --release
target\release\pencil.exe
```

It stays in the background with a tray icon (right click → *Draw*, *Settings*, *Quit*).
Only one instance can run at a time.

## Usage

Default shortcuts (all but the fixed ones can be changed in *Settings*):

| Action | Shortcut |
|---|---|
| Show / hide overlay | **Ctrl + Alt + D** (global) |
| Pencil: cycle color (blue → red → black → white) | click the pencil, `P` |
| Highlighter: cycle color (yellow → green → pink → blue) | click the marker, `H` |
| Size (4 presets, 2nd by default; pencil and highlighter) | click the dot, `S` |
| Clear everything | eraser, `E`, `Del` and `Backspace` (fixed) |
| Size presets | `1`–`4` (fixed) |
| Undo (also brings back a cleared drawing) | `Ctrl+Z` (fixed) |
| Redo | `Ctrl+Y` or `Ctrl+Shift+Z` (fixed) |
| Screenshot of an area | camera button, `C` (`Esc` cancels the selection) |
| Close overlay | X, `Esc` (fixed), `Alt+F4`, or the global hotkey |

## Screenshots

Press the camera button (or `C`), then drag over the area to capture, or just
click to capture the whole monitor under the pointer. Hold `Ctrl` to point at
windows instead: the one under the pointer is highlighted and a click captures
it. The image
keeps your drawings but never the toolbar. A shutter sound plays, and the image is copied to the clipboard and
saved as a PNG in `Pictures\Pencil`; a notice above the toolbar confirms it
(click it to open the image). You can keep drawing afterwards.

## Settings

Right click the tray icon → *Settings* opens a small native window to change the
show/hide hotkey (must include Ctrl or Alt), the color/size/erase/capture keys, and
the toolbar theme (System, Light or Dark; System follows the Windows app mode).
Settings are stored under `HKCU\Software\Pencil`.

The overlay also closes by itself if it loses focus (Alt+Tab, Windows key,
Ctrl+Alt+Del, session lock) or if the display configuration changes.
Closing it clears the drawing and frees all of its memory.

## Resources

- Idle: ~1.5 MB private memory, 0 % CPU (the thread sleeps in `GetMessageW`).
- With the overlay: a single bitmap the size of the virtual desktop (4 bytes/pixel);
  only the modified rectangle is sent to the compositor on each move.
- No GPU work of its own: composition is done by DWM, like any other window.

## Releasing a new version

On GitHub: *Actions* > *Release* > *Run workflow*, and pick the bump
(`patch`, `minor`, `major`, or `current` to publish the version as it is).
From a terminal: `gh workflow run release.yml -f bump=patch`.

The workflow bumps `version` in `Cargo.toml`, builds the app, packages
`installer/pencil.iss` with Inno Setup, commits the bump, tags it `vX.Y.Z` and
publishes `PencilSetup.exe` as a GitHub release. Installed copies pick it up the
next time they start.
