# SettingsGoo

A desktop settings app for Linux that works under both X11 and Wayland, independent of any particular window manager or compositor.

By AI Sloperator with Claude Code — <https://www.aisloperator.com/>

Portions Copyright AI Sloperator. No license is given.

## Status

First version. Written in Rust with GTK 4. The only crate dependency is `gtk4` (and the `glib`/`gio`/`pango` bindings it brings in).

## Pages

| Page | What it does | How |
| --- | --- | --- |
| Wi-Fi | Turn Wi-Fi on/off; list nearby networks with signal strength and security; connect (asks for a password for WPA/WPA2/WPA3 networks), disconnect, forget saved networks; connection details | NetworkManager over D-Bus |
| Network | Wired devices, plus Bluetooth, mobile broadband, WireGuard, bond, VLAN and team devices: status, connect/disconnect, IP addresses, gateway, DNS, speed, hardware address | NetworkManager over D-Bus |
| Appearance | Color scheme (light/dark), GTK theme, icon theme, cursor theme and size, interface font | The `org.gnome.desktop.interface` GSettings keys, if that schema is installed (read by GTK, `xdg-desktop-portal` and others); `~/.config/gtk-3.0/settings.ini` and `~/.config/gtk-4.0/settings.ini`; `~/.icons/default/index.theme` for the XCursor fallback |
| Displays | Lists connected monitors: connector, model, resolution, scale, position, refresh rate, physical size (read-only) | GDK |
| Sound | Default output and input device, volume, mute | `pactl` (works with PulseAudio and with PipeWire through pipewire-pulse) |
| Region & Keyboard | System language (`LANG`); primary XKB keyboard layout and variant | systemd-localed (`org.freedesktop.locale1`) over D-Bus |
| Date & Time | Time zone, automatic network time | systemd-timedated (`org.freedesktop.timedate1`) over D-Bus |
| About | Device name and hostname (editable); OS, kernel, hardware, CPU, memory, desktop, windowing system | systemd-hostnamed (`org.freedesktop.hostname1`), `/proc`, `/etc/os-release` |

System-wide changes (time zone, language, keyboard, hostname) go through polkit, so you may be asked for an administrator password. That needs a polkit authentication agent running in the session.

If a service or tool is missing, its page says so instead of failing.

### Not included (yet)

Wi-Fi networks using enterprise (802.1X) or WEP security, hidden networks, and editing connection settings (such as static IP addresses) are not supported yet.

Settings with no standard interface shared by X11 and the various Wayland compositors are left out for now. These include monitor arrangement and resolution, pointer and touchpad settings, keyboard repeat rate, and per-session keyboard layouts. They can be added once the window manager or compositor is chosen.

## Keyboard shortcuts

- Ctrl+Q — quit
- Ctrl+W — close the window

## Building

Requires Rust and the GTK 4 development files (GTK 4.14 or newer).

```sh
cargo build --release
./target/release/settingsgoo
```

Runtime tools used if present: `pactl` (Sound page) and `locale` (list of installed locales).

## Code layout

- `src/main.rs` — application, window, sidebar
- `src/sys.rs` — D-Bus and command helpers, row/group layout helpers, `Choice` (a drop-down) and `Toggle` (a switch). Both undo the change and show an error if applying it fails.
- `src/nm.rs` — a small NetworkManager D-Bus client
- `src/pages/*.rs` — one module per page
