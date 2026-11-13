# lanlink

A small KDE Connect alternative: one Rust binary with a daemon and a TUI, no
KDE or GNOME libraries. It speaks the KDE Connect protocol, so it pairs with
the [lanlink Android app](https://github.com/adelmonte/lanlink-android), the
stock KDE Connect app, and other machines running lanlink.

- **Files, links and text** both ways. Received files go to `~/Downloads`.
- **Clipboard on request**: `lanlink clip <device>` sends this clipboard to the other
  device, and text sent here lands on the clipboard. Nothing syncs on its own.
- **Browse the phone**: its storage mounts with `sshfs` under `$XDG_RUNTIME_DIR/lanlink/<name>`.
- **Media**: the phone controls desktop players (MPRIS), from its own media controls.
- **Recognized songs**: with the app's Audile setting on, each song Audile finds is
  passed to `song_command` (by default `music-tui --like`, which adds it to Liked Songs).
- **Remote input**: the phone, or another machine, drives the mouse and keyboard
  through `/dev/uinput` (any compositor, X11 too). `lanlink remote <device>` does
  the reverse and drives another machine from this keyboard; Ctrl+Alt+Esc releases.

Not included: SMS, calls, notification sync, automatic clipboard sync, screen sharing.

## Install

Arch (AUR):

```sh
yay -S lanlink
```

From source:

```sh
cargo build --release
install -Dm755 target/release/lanlink ~/.local/bin/lanlink
```

`sshfs` is needed for phone browsing and `wl-clipboard` (or `xclip` on X11) for the clipboard. For remote input, give the `input` group
access to uinput (the AUR package installs these two files):

```sh
sudo install -Dm644 contrib/60-lanlink-uinput.rules /etc/udev/rules.d/60-lanlink-uinput.rules
sudo install -Dm644 contrib/uinput.conf /etc/modules-load.d/uinput.conf
sudo usermod -aG input "$USER"        # then log out and back in
```

Start the daemon with your session. With systemd (a source install also needs
`contrib/lanlink.service` copied to `~/.config/systemd/user/`):

```sh
systemctl --user enable --now lanlink
```

Without it (dinit, runit, ...), run `lanlink daemon` from your compositor, e.g. niri:

```kdl
spawn-at-startup "lanlink" "daemon"
```

If you run a firewall, allow TCP and UDP 1714-1764 on the LAN.

## Use

```sh
lanlink                      # TUI
lanlink devices
lanlink pair pixel           # any unique prefix of a name or id
lanlink send pixel photo.jpg https://example.com
lanlink clip pixel           # send the clipboard (bind it to a key)
lanlink mount pixel
lanlink remote tv            # Ctrl+Alt+Esc to stop
lanlink connect 192.168.1.20 # if discovery doesn't find it
lanlink help
```

Pair requests also arrive as a desktop notification with Accept/Reject buttons.

## Config

`~/.config/lanlink/config.toml`; see [contrib/config.toml](contrib/config.toml).
The device identity lives next to it and paired devices are in `trusted/`;
deleting a file there unpairs that device.

## License

GPL-3.0-or-later
