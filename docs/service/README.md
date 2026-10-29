# Running the daemon as a service

A client starts the daemon on demand (ADR 0022), so none of this is required. These templates start it with the login instead. They are not installed by anything; copy them by hand. Each unit is for one protocol version: the `1` in the paths and names is that version.

## systemd (Linux)

[`bitplane-sessiond.socket`](bitplane-sessiond.socket) and [`bitplane-sessiond.service`](bitplane-sessiond.service) are user units. systemd owns the socket and starts the daemon on the first connection, handing the socket over through `LISTEN_FDS`.

```sh
# Install both units for the current user
cp bitplane-sessiond.socket bitplane-sessiond.service ~/.config/systemd/user/
# Fix ExecStart if the binary is not in ~/.local/bin
systemctl --user daemon-reload
systemctl --user enable --now bitplane-sessiond.socket
```

The user's systemd manager stops at logout, and every session with it. To keep sessions running with no one logged in:

```sh
# Keep the user manager running after logout
loginctl enable-linger "$USER"
```

## launchd (macOS)

[`io.bitplane.sessiond.v1.plist`](io.bitplane.sessiond.v1.plist) is a user agent that runs `serve` at login. The daemon binds its own socket: launchd hands sockets over through `launch_activate_socket`, not `LISTEN_FDS`, and `serve` does not support that.

There is no `KeepAlive`. A daemon that is already running makes this one exit at once, and launchd would restart it forever. When the daemon dies, the next client starts it again.

```sh
# Replace /Users/YOU in the plist, then load it
cp io.bitplane.sessiond.v1.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/io.bitplane.sessiond.v1.plist
```

launchd starts agents without `TMPDIR`; the daemon then uses the same per-user temp dir that `TMPDIR` names in a login session, so clients still find the socket.

## Environment

However it is started, the daemon reads the login-shell environment once at startup (`$SHELL -l -i`), so sessions and tools see the user's `PATH`. Changes to rc files take effect after a daemon restart.
