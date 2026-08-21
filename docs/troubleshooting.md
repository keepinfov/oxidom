# Troubleshooting

Organised by what you see. Error text is quoted as oxidom actually prints it.

## Contents

- [My servers vanished](#my-servers-vanished)
- [The core will not start](#the-core-will-not-start)
- [It says connected but nothing works](#it-says-connected-but-nothing-works)
- [Every server fails its latency check](#every-server-fails-its-latency-check)
- [Subscriptions](#subscriptions)
- [TUN and `oxidom run`](#tun-and-oxidom-run)
- [The GUI](#the-gui)
- [Files moved aside](#files-moved-aside)
- [Getting logs](#getting-logs)

## My servers vanished

Almost always: you are talking to a **different daemon than last time**.

A session daemon keeps its database in `~/.config/oxidom` and
`~/.local/share/oxidom`. The system daemon keeps everything in `/var/lib/oxidom`.
They do not share servers, subscriptions or profiles. See
[configuration.md](configuration.md#the-two-databases).

Check which one is answering:

```sh
systemctl status oxidom.service
busctl --system list | grep oxidom     # system daemon owns the name?
busctl --user   list | grep oxidom     # or a session one?
```

If the system daemon should be in charge but a session daemon answered, the usual
cause is that the client could not reach the system one:

- The unit is not running — `sudo systemctl enable --now oxidom.service`.
- You are **not allowed** to drive it. Only `root`, `wheel` and the `oxidom` group
  may. `AccessDenied` is a final answer, and oxidom correctly gives you a session
  daemon of your own instead. To join the group:
  `services.oxidom.users = [ "alice" ];` on NixOS, `gpasswd -a alice oxidom`
  elsewhere — then log out and back in.

Servers added to one database can be re-imported into the other from the same
subscription URL.

## The core will not start

### `xray` cannot be found or run

```
`xray` was not found on $PATH (looked in …) — install xray,
or set its full path in Settings
```

```
the Xray binary from Settings › Xray binary does not exist: /usr/bin/xray
the Xray binary … is not executable (mode 0644)
the Xray binary … is a directory, not a program
```

The message names both the path tried and **where that path came from** — the
config key, the environment variable, or `PATH`. Fix it at that source. Resolution
order is `xray_binary` → `$OXIDOM_XRAY_BIN` → `PATH`; see
[configuration.md](configuration.md#finding-helper-binaries).

### The core cannot load `geoip.dat`

```
Failed to start: main: failed to load config files: [...]
  > infra/conf: failed to build routing configuration
  > infra/conf: invalid field rule
  > infra/conf: failed to load GeoIP: private
  > infra/conf: failed to open file: geoip.dat
```

Nothing is wrong with the server or with your settings: the core cannot find its
geo data. Every configuration oxidom generates carries the built-in
`geoip:private` and `geosite:private` references, so a core without the lists
refuses **every** connection, not just ones with routing rules.

This happens when the core was installed by hand, because the Xray release zip
contains the binary and nothing else. Nix and the AUR's `xray-bin` both supply the
files.

Install them where every Xray build looks:

```sh
curl -LO https://github.com/v2fly/geoip/releases/latest/download/geoip.dat
curl -Lo geosite.dat https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat
sudo install -Dm644 geoip.dat   /usr/local/share/xray/geoip.dat
sudo install -Dm644 geosite.dat /usr/local/share/xray/geosite.dat
```

Note the rename: upstream publishes `geosite.dat` under the name `dlc.dat`.

To check a core can load them, without connecting anything:

```sh
printf '%s' '{"outbounds":[{"protocol":"freedom","tag":"direct"}],
  "routing":{"rules":[{"type":"field","ip":["geoip:private"],"outboundTag":"direct"}]}}' > /tmp/geo-test.json
xray run -test -c /tmp/geo-test.json     # "Configuration OK." means the data loaded
```

If the files are somewhere else, point the core at that directory with
`XRAY_LOCATION_ASSET`. See
[installation.md](installation.md#getting-the-geo-data).

### A port is taken

```
local SOCKS endpoint 127.0.0.1:10808 is already in use — pick a different
port in Settings
```

Either something else has it, or another oxidom session does. Sessions each get
their own loopback address specifically so they do not collide, so this usually
means a foreign program:

```sh
ss -ltnp | grep 10808
```

### Hysteria2 exits immediately

```
the core does not support this server's protocol — hysteria2 needs Xray 26.1
or newer, which is where the native "hysteria" protocol landed; an older core
exits immediately instead of connecting
```

Check with `xray version`. Nothing else will fix this but a newer core.

## It says connected but nothing works

oxidom confirms a tunnel after connecting and tears it down if it cannot prove it
works, so these messages are the account of *why* it was torn down:

| Message | Meaning |
|---|---|
| `the local SOCKS inbound never came up — the core is not carrying traffic` | The core started but never bound its inbound. Check the Logs page. |
| `active server did not pass its latency check` | The core is up; the server did not answer through it. |
| `the pool carried no traffic within 20s — 2 of 8 nodes were in rotation` | The pool's members are mostly unreachable. |
| `the pool carried no traffic within 20s — its health check could not be reached through 8 of 8 nodes — the address is [core] pool_probe_url` | The members may be fine. The balancer only puts a node in rotation once it has reached the health-check address through it, and that address answered through none of them. Point `[core] pool_probe_url` at something reachable from where you are — see [configuration.md](configuration.md#pool_probe_url). |
| `Xray exited unexpectedly` | The core died. The reason is in the log buffer. |

Things that look like a broken tunnel but are not:

- **TLS failures on a link that asked for `allowInsecure`.** Xray 26.x removed it;
  the certificate is now verified normally, and a self-signed certificate will
  fail. A certificate pin (`pinSHA256`) is the only escape hatch.
- **An unknown obfuscation type** stops the core starting outright:
  `ignoring unknown "<kind>" obfuscation`.
- **TUN enabled but nothing routed.** `routes = "manual"` is the default and
  captures nothing on its own. See
  [routing.md](routing.md#what-routes-actually-does).
- **DNS.** oxidom never touches `/etc/resolv.conf`. With `routes = "default"` your
  resolver may still be reached outside the tunnel — this is warned about at
  connect time.

Confirm what the world sees:

```sh
oxidom ip --egress --fresh
```

## Every server fails its latency check

The `⊘` badge never means "this server is dead". It means the check never reached
the server, which is this machine's problem — and a machine that cannot measure
fails *every* server at once, which is why a whole subscription can turn to `⊘`
in one sweep. Replacing servers will not help.

The badge's tooltip names the condition, and `oxidom ping <HANDLE>` prints the
same reason on stderr:

| Message | What to do |
|---|---|
| `no Xray core, so nothing could be measured` | No core binary was found. `oxidom core show` prints where it looked; install one, or set the path in Settings › Xray core. |
| `the server's certificate was rejected` | The server's TLS certificate did not verify. `oxidom trust <HANDLE>` shows it, and accepts it with `--trust` if you recognise it. |
| `the server asks for unverified TLS, which this core removed` | The share link asked for `allowInsecure`, which Xray 26.x dropped. A certificate pin (`pinSHA256`) is the only way through. |
| `the core refused the generated config` | The core would not start on this server's settings — usually an option this core version does not have. Connecting to the server will show the core's own words; a probe's core is read once and discarded. |
| `the core has no geo data (geoip.dat, geosite.dat), so it refused the routing rules` | The core cannot load `geoip.dat`/`geosite.dat`. Nothing about the server is wrong — every generated config needs the lists. See [The core cannot load `geoip.dat`](#the-core-cannot-load-geoipdat). |
| `the check could not run on this machine` | A local fault the core did not name: no free port, or an unwritable data directory. |
| `no network connection` (the badge says `No network — the server was not checked`) | This machine has no usable default route. Claimed only on evidence, never guessed from a DNS failure alone, so it is worth believing. |

A single server showing `⊘` while its neighbours show numbers is the certificate
or config case, not the missing-core one — and never the geo-data one, which
fails every server at once because the lists are not per-server.

Note that a check runs a **throwaway core of its own** for each server it
measures directly — so a broken core binary breaks measuring even while an
already-running tunnel keeps working.

## Subscriptions

| Message | Fix |
|---|---|
| `the panel sent a web page instead of a server list — it may not recognize this app; try another Client preset in Settings` | Change the User-Agent. Panels gate the body on it. |
| `subscription returned no supported servers (expected a share-link list, Xray or sing-box JSON, or Clash YAML)` | The format is not one oxidom reads. |
| `this subscription requires a device identifier; enable Advanced > Send HWID and add it again` | Opt in per subscription — off by default on purpose. |
| `fetching subscription: the server responded with HTTP 403` | Usually the User-Agent or an expired subscription. The URL is deliberately not echoed — it is a credential. |
| `shadowsocks link requests a SIP003 plugin, which is not supported` | Xray cannot run plugins. That server will not work. |

If a refresh disconnected you: `the active server is no longer in its subscription
— disconnected`. That is deliberate. oxidom will not silently move you to a server
you did not choose. Pick another, or use a [rule pool](profiles-and-pools.md#lists-and-rules)
that follows the subscription.

## TUN and `oxidom run`

### `profile 'work' asks for a network interface, but this daemon has no CAP_NET_ADMIN`

Interfaces come only from the system daemon. On NixOS:

```nix
services.oxidom.enable = true;
services.oxidom.tun.enable = true;
```

Elsewhere, the unit needs `AmbientCapabilities=CAP_NET_ADMIN` —
`sudo systemctl edit oxidom.service`. The AUR unit does not set it.

oxidom will not escalate on its own, by design.

### `no systemd --user manager is available for uid 1000`

`oxidom run` needs a user systemd session. Over plain SSH or in a container there
often is not one. `oxidom env` needs none — use it for anything that honours proxy
variables.

### `profile 'work' has no network interface`

`oxidom run` requires `[interface] enable = true` on that profile, and the
interface to be up. The message points at `oxidom env` for the simpler path.

### `refusing to delete network interface "oxi-work": oxidom did not create it`

Working as intended — a device oxidom did not create is never deleted. Remove it
yourself if you are sure.

### `nft (…) exited with … while updating table inet oxidom`

The `nft` binary is missing or the daemon lacks the privilege to use it. Only
`oxidom run` needs it. Note the AUR package does not depend on `nftables` or
`tun2socks` — install them if you use these features.

## The GUI

- **No tray icon.** The tray needs a StatusNotifierItem host. On stock GNOME that
  means an AppIndicator extension; without one the window still works.
- **System proxy does nothing.** It is GNOME-specific (`gsettings` on
  `org.gnome.system.proxy`). Elsewhere: `running gsettings (is this a GNOME
  session?)`.
- **A stuck system proxy** after a crash is repaired on the next GUI start. To fix
  it by hand: `gsettings set org.gnome.system.proxy mode none`.
- **Missing icon after a source install.** Debug builds drop the icons into
  `$XDG_DATA_HOME` themselves; release builds do not. Install the assets — see
  [installation.md](installation.md).
- **Closing the window does not disconnect.** That is intentional; the daemon owns
  the tunnel. Use `oxidom down`, or Disconnect.

## Files moved aside

```
config.toml is not valid (…); moved aside to …/config.toml.corrupt-1753970400
```

A file that cannot be parsed is **never overwritten**. It is renamed and oxidom
continues with defaults. Your data is still there — inspect the `.corrupt-*` file,
fix it, and move it back with the daemon stopped.

## Getting logs

The daemon logs to stderr, so under systemd:

```sh
journalctl -u oxidom.service -f          # system daemon
journalctl --user -f | grep oxidom       # session daemon
```

The GUI's **Logs** page shows the Xray core, the network interface helper and
oxidom's own reasoning in one stream, each line tagged with which of the three
said it. Filter by source, hide anything below a chosen severity, or search the
text. **Save** writes what is on screen — filters included — wherever you choose.

Scroll up and the view stops following; new lines keep arriving below without
moving what you are reading. The button at the bottom right takes you back to
live.

The graphical client also keeps its own log on disk, because it detaches from the
terminal and would otherwise leave nothing behind if it were killed:

```sh
~/.local/share/oxidom/oxidom-gui.log     # 0600, rotated at 2MB (.log.1 kept)
```

The daemon has no such file — its stderr already reaches the journal above.

Raise the level with `RUST_LOG`:

```sh
RUST_LOG=debug oxidom daemon
oxidom-gui --debug          # foreground, debug level
```

`RUST_LOG` overrides the default either way (`info` for `daemon`, `warn` for other
commands).

### Exit codes for scripts

`0` success, `1` command error, `3` not connected, `4` no daemon reachable.
Exit 3 prints nothing — it is "nothing to report", not a failure. See
[cli.md](cli.md#exit-codes).

---

Next: [cli.md](cli.md) · [configuration.md](configuration.md) · [routing.md](routing.md)
