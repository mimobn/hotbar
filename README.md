# hotbar

Nine numbered clipboard slots on top of your normal clipboard, plus clipboard
history. One small binary for Linux (Wayland and X11), Windows and macOS.

```
Ctrl+C          copy, like always
Ctrl+C then 1   copy, and also keep it in slot 1
Ctrl+V          paste, like always
Ctrl+V then 1   paste slot 1
Super+V         history picker (Ctrl+Alt+V on Windows)
```

Think of a game hotbar: you keep the things you paste all day (an email
address, a server command, a snippet) in slots 1 to 9, and they stay there
across copies and reboots. Your normal clipboard keeps working as before.

On macOS use Cmd instead of Ctrl, and Cmd+Shift+V for the picker.

## How the chord works

Hold Ctrl, tap V, let go of V, tap a digit. You don't have to hold V and the
digit together.

- A plain paste goes out the moment you release Ctrl or press any other key.
  There is no timer, so normal pasting doesn't feel slower, and typing numbers
  after a paste never triggers a slot.
- Ctrl+1 on its own (switching browser tabs, for example) is not touched.
- Ctrl+Shift+V then a digit works in terminals.
- After a slot paste, whatever was on the clipboard before is put back, so
  your clipboard isn't overwritten.

If you'd rather let go of every key and then press the digit, set
`chord = "release"`. Normal pastes then wait `timeout_ms` (300 ms by default).

## Why another clipboard manager

The usual Wayland setup is cliphist plus a rofi or fuzzel keybind. hotbar
keeps that workflow (same `list` and `decode` pipes) and adds:

|                                | hotbar | cliphist | clipvault | clipse |
|--------------------------------|--------|----------|-----------|--------|
| Numbered slots, pinned forever | yes    | no       | no        | no     |
| Paste a slot with one chord    | yes    | no       | no        | no     |
| Wayland                        | yes    | yes      | yes       | yes    |
| X11, Windows, macOS            | yes    | no       | no        | no     |
| Runs without `wl-paste --watch`| yes    | no       | no        | no     |
| Images (in history and slots)  | yes    | yes      | yes       | yes    |
| Built-in picker window         | yes    | no       | no        | TUI    |

Also:

- Copies that password managers mark as secret are skipped by default.
- On Linux, the clipboard stays available after the app you copied from closes.
- History limits: max items, max age, min and max length, image size.
- One binary with no runtime dependencies, and a single config file that
  reloads itself.

## Install

From source (needs Rust):

```sh
cargo install --git https://github.com/mimobn/hotbar
```

Release builds for all three platforms are attached to each GitHub release.

### Linux

hotbar reads your keyboard through `/dev/input` and types through
`/dev/uinput`, the same way keyd and kanata do. That works on X11 and on every
Wayland compositor, but your user needs access once:

```sh
hotbar setup        # shows the sudo commands and asks before running them
```

Log out and back in, then check:

```sh
hotbar doctor
```

Start it with your session:

```sh
hotbar autostart on   # systemd user service
```

or in your compositor config:

```ini
# Hyprland
exec-once = hotbar daemon
windowrulev2 = float, class:^(hotbar)$
windowrulev2 = pin, class:^(hotbar)$

# sway / i3
exec hotbar daemon
for_window [app_id="hotbar"] floating enable
```

The clipboard side uses the wlr data-control protocol on Wayland (Hyprland,
Sway, KDE Plasma, niri, river, Wayfire, labwc and others) and XFixes on X11.
GNOME does not offer data-control, so on GNOME hotbar falls back to XWayland
and only sees copies from X11 apps.

#### Without key grabbing

If you'd rather keep everything in your window manager config, turn the
chords off and bind the commands yourself:

```toml
# ~/.config/hotbar/config.toml
[keys]
chord = "off"
picker = ""
```

```ini
# Hyprland
bind = SUPER, V, exec, hotbar pick
bind = CTRL ALT, 1, exec, hotbar paste 1
bind = CTRL ALT SHIFT, 1, exec, hotbar save 1
```

#### Coming from cliphist

The pipes look the same, so rofi scripts carry over:

```sh
hotbar list | rofi -dmenu | hotbar decode | wl-copy
hotbar list | fuzzel --dmenu | hotbar copy
```

Or have the picker hotkey open your menu of choice:

```toml
[picker]
command = "fuzzel --dmenu"
```

### Windows

Nothing to set up. Run `hotbar daemon`, or `hotbar autostart on` to start it
at login (it uses `hotbarw.exe`, which has no console window).

Ctrl+Alt+V opens hotbar's picker. Win+V stays the Windows clipboard, so both
work side by side. Saving to a slot, or pasting an empty one, shows a small
popup near the bottom of the screen that never takes focus.

### macOS

macOS asks for two permissions in System Settings > Privacy & Security:
Accessibility (to send the paste keystroke) and Input Monitoring (to see Cmd+V
followed by a digit). `hotbar setup` opens both pages.

```sh
hotbar autostart on   # LaunchAgent
```

## Picker

`Super+V` on Linux, `Ctrl+Alt+V` on Windows, `Cmd+Shift+V` on macOS, or run
`hotbar pick`. Press the hotkey again to close it.

The top row is your hotbar: the nine slots, filled or empty, with a
thumbnail for images. Click a slot to paste it. History is listed below, and
entries held in a slot show its number.

| Key            | Does                                          |
|----------------|-----------------------------------------------|
| type           | search                                        |
| Up, Down       | move                                          |
| Enter, click   | paste                                         |
| 1 to 9         | paste that slot (while the search is empty)   |
| Alt+digit      | put the selected entry in that slot           |
| Delete         | delete the entry (Shift+Delete while searching) |
| Esc            | close                                         |

## Commands

```
hotbar daemon            run in the background
hotbar stop              stop the daemon
hotbar save N            current clipboard into slot N
hotbar paste N           paste slot N into the focused window
hotbar get N             print slot N
hotbar set N [TEXT]      fill slot N from TEXT or stdin (text or PNG)
hotbar clear N           empty slot N
hotbar slots             show the nine slots
hotbar list              history as "id<TAB>preview", newest first
hotbar decode [ID]       print an entry (id or a list line on stdin)
hotbar copy [ID]         put an entry back on the clipboard
hotbar delete [ID]       delete an entry
hotbar pick              open the picker (--dmenu "cmd" for any dmenu)
hotbar wipe [--slots]    delete history (slots kept unless --slots)
hotbar setup             permissions for key chords (Linux, macOS)
hotbar doctor            show what works on this machine
hotbar autostart on|off  start at login
hotbar config [--init]   show the config path, or write the default config
```

Slot and history commands also work when the daemon is not running.

## Config

You don't need a config file: the defaults work. To change something, run
`hotbar config --init`. It writes a short file with the options people
usually change ([packaging/config.toml](packaging/config.toml)), for example:

```toml
[keys]
picker = "ctrl+alt+v"
```

The daemon reloads the file when you save it. Every other option is listed
below; add only the ones you want.

| Section     | Option              | Default       | Meaning |
|-------------|---------------------|---------------|---------|
| `[keys]`    | `chord`             | `"ctrl-held"` | `"ctrl-held"`, `"release"` or `"off"` |
|             | `picker`            | per platform  | picker hotkey, `""` to turn it off |
|             | `timeout_ms`        | `300`         | wait for the digit in release mode |
|             | `primary`           | `"auto"`      | `"ctrl"` or `"cmd"`; auto is Cmd on macOS |
|             | `copy_key`          | `"c"`         | physical copy key (Dvorak on Linux: `"i"`) |
|             | `paste_key`         | `"v"`         | physical paste key (Dvorak on Linux: `"."`) |
|             | `numpad`            | `true`        | keypad digits pick slots too |
| `[history]` | `max_items`         | `500`         | entries kept; slots never count |
|             | `max_age_days`      | `0`           | 0 keeps entries forever |
|             | `min_length`        | `1`           | ignore shorter text |
|             | `max_length`        | `0`           | ignore longer text, 0 for no limit |
|             | `max_image_mb`      | `20`          | ignore bigger images |
|             | `ignore_sensitive`  | `true`        | skip password manager copies |
|             | `ignore_blank`      | `true`        | skip whitespace-only copies |
| `[paste]`   | `restore_clipboard` | `true`        | put the old clipboard back after a slot paste |
|             | `restore_delay_ms`  | `300`         | how long to wait before restoring |
|             | `notify`            | `true`        | notification when a slot is saved |
| `[picker]`  | `command`           | `"builtin"`   | or a dmenu command like `"fuzzel --dmenu"` |
|             | `width`, `height`   | `640`, `460`  | window size |
|             | `close_on_unfocus`  | `true`        | close when you click elsewhere |
| `[theme]`   | `pywal`             | `false`       | colors from `~/.cache/wal/colors.json` |
|             | `background`, `surface`, `foreground`, `muted`, `accent` | dark theme | `"#rrggbb"` or `"#rrggbbaa"` |
|             | `radius`, `font_size` | `12`, `15`  | corner radius and text size |

## Files

| What    | Linux                         | macOS                                    | Windows                     |
|---------|-------------------------------|------------------------------------------|-----------------------------|
| config  | `~/.config/hotbar/config.toml` | `~/.config/hotbar/config.toml`           | `%APPDATA%\hotbar\config\config.toml` |
| history | `~/.local/share/hotbar/`       | `~/Library/Application Support/hotbar/`  | `%APPDATA%\hotbar\data\`    |

History is plain files: `index.json` plus one file per entry in `blobs/`.
`HOTBAR_CONFIG` and `HOTBAR_DATA` override both paths.

## Privacy

- Copies marked secret by password managers are not saved: the
  `x-kde-passwordManagerHint` type on Linux, `ExcludeClipboardContentFromMonitorProcessing`
  on Windows, and the nspasteboard.org types on macOS. Password managers such
  as KeePassXC set these when they copy a password.
- Slot pastes and clipboard restores are marked the same way, so other
  clipboard managers (and Windows clipboard history) don't record them.
- Nothing leaves your machine. The daemon socket is only readable by your user.

## Manual test checklist

The chord engine has unit tests, and CI builds and tests on all three
platforms. Keyboard and clipboard behavior on a real desktop still needs a
person:

1. `hotbar daemon`, then `hotbar doctor` shows no FAIL lines.
2. Select text, Ctrl+C then 1. `hotbar slots` shows it in slot 1.
3. Copy something else. Ctrl+V then 1 pastes slot 1, and a plain Ctrl+V right
   after still pastes the other thing.
4. Ctrl+1 in a browser still switches tabs.
5. In a terminal, Ctrl+Shift+V then 1 pastes slot 1.
6. The picker hotkey (Super+V, Ctrl+Alt+V on Windows) opens the picker,
   Enter pastes, Esc closes. On Windows, Win+V still opens the Windows clipboard.
7. Copy from a password manager: it is not in `hotbar list`.

## Building

```sh
cargo build --release
cargo test
```

Linux builds need no system libraries: the Wayland, X11 and evdev code is pure
Rust.

## License

MIT
