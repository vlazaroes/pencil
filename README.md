# Pencil

Full-screen drawing overlay for Windows, in pure Rust (only `windows-sys`).

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
| Size (4 presets, 2nd by default) | click the dot, `S` |
| Clear everything | eraser, `E`, `Del` and `Backspace` (fixed) |
| Size presets | `1`–`4` (fixed) |
| Close overlay | X, `Esc` (fixed), `Alt+F4`, or the global hotkey |

## Settings

Right click the tray icon → *Settings* opens a small native window to change the
show/hide hotkey (must include Ctrl or Alt), the pencil/size/clear keys, and the
toolbar theme (System, Light or Dark; System follows the Windows app mode).
Settings are stored under `HKCU\Software\Pencil`.

The overlay also closes by itself if it loses focus (Alt+Tab, Windows key,
Ctrl+Alt+Del, session lock) or if the display configuration changes.
Closing it clears the drawing and frees all of its memory.

## Resources

- Idle: ~1.5 MB private memory, 0 % CPU (the thread sleeps in `GetMessageW`).
- With the overlay: a single bitmap the size of the virtual desktop (4 bytes/pixel);
  only the modified rectangle is sent to the compositor on each move.
- No GPU work of its own: composition is done by DWM, like any other window.
