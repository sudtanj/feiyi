# feiyi for macOS (Apple Silicon)

One binary, `feiyi`, with the web app built in. It runs on Apple Silicon Macs (M1 and later). The
build is unsigned: no Apple certificate was used, so Gatekeeper blocks it until you clear the
quarantine flag.

## Install and run

```sh
tar xzf feiyi-aarch64-apple-darwin.tar.gz
xattr -d com.apple.quarantine feiyi    # only needed if you downloaded it with a browser
PASEO_PASSWORD='choose-a-password' ./feiyi
```

Open <http://localhost:6768/>. The page is the feiyi web app, and it adds the daemon it was served
from as a host. If the host list is empty, add it by hand: Add host, direct connection, host
`localhost`, port `6768`, and the password you set. The WebSocket endpoint is `ws://localhost:6768/ws`.

The pairing page (link and QR for the phone app through the relay) moved to
<http://localhost:6768/pair>. `./feiyi pair` still prints the link in the terminal.

To check the download first: `shasum -a 256 -c feiyi-aarch64-apple-darwin.tar.gz.sha256`.

## Requirements

`claude` and/or `codex` must be on `PATH` and logged in. A daemon started from a GUI launcher does
not inherit your shell `PATH`, so start it from a terminal.

## State

Everything lives in `~/.paseo-lite` (config, agent history, uploads). Set `PASEO_LITE_HOME` to move
it. Delete the directory to start clean.

## Settings

`./feiyi help` lists the environment variables. Common ones:

| Variable | Effect |
|---|---|
| `PASEO_PASSWORD` | password for direct connections (set it) |
| `PASEO_LISTEN` | listen address, default `127.0.0.1:6768` (loopback only) |
| `PASEO_RELAY_ENABLED=false` | no relay connection, local use only |
| `PASEO_CLAUDE_MAX_LIVE=<n>` | fix the number of concurrent `claude` processes |

## Capacity on macOS

feiyi sizes the number of concurrent agent processes from total memory (read with
`sysctl hw.memsize`) and the CPU count. macOS has no cgroup or `/proc` memory reading, so there is
no automatic shrinking under memory pressure: the limit stays at the computed ceiling. Set
`PASEO_CLAUDE_MAX_LIVE=<n>` to choose a number yourself.
