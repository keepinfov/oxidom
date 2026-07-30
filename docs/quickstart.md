# Quickstart

From nothing to a verified tunnel. Assumes oxidom is
[installed](installation.md) and an `xray` binary is available.

## With the GUI

1. **Launch it.**

   ```sh
   oxidom-gui
   ```

   If no daemon is running, one is started for you.

2. **Add a subscription.** *Subscriptions* → **Add subscription**, paste the URL.

   You can also paste plain share links — one, or many at once, one per line.
   They land in a group called **My servers**.

   If the panel returns a web page instead of servers, it did not recognise the
   client. Change *Settings › Subscription User-Agent* (there is a **Client
   preset** list) and refresh. This is common and expected.

3. **Pick a server** on the *Servers* page and press **Connect**.

   The card shows a latency once measured. oxidom verifies the tunnel actually
   carries traffic before reporting success, so "connected" means connected.

4. **Use it.** The tunnel is a local SOCKS5 + HTTP proxy. To send the whole
   desktop through it, turn on *Settings › System proxy* (GNOME).

Closing the window does **not** disconnect — the daemon owns the tunnel. Use
**Disconnect**, or `oxidom down`.

## With the CLI

```sh
# 1. See what the daemon knows
oxidom list servers

# 2. Give a server a handle you will remember
oxidom alias 3f2a91c4b7e05d16 ch-trojan

# 3. Connect
oxidom connect ch-trojan

# 4. Check
oxidom status
# connected  ch-trojan  Zurich 01  socks 127.0.0.1:10808  84 ms

# 5. Prove it
oxidom ip --egress
# 203.0.113.7
```

Send a program through it:

```sh
eval "$(oxidom env)"
curl https://ifconfig.me
```

Stop:

```sh
oxidom down
```

Adding subscriptions is currently a GUI action; the CLI reads and connects.

## Making it a profile

A [profile](profiles-and-pools.md) is a saved connection — a server, its ports, and
optionally a network interface.

```sh
oxidom profile new work
oxidom profile edit work        # opens $EDITOR
```

```toml
description = "work"

[select]
server = "ch-trojan"
```

The editor validates before saving, so a profile that could not connect is
rejected here rather than at connect time.

```sh
oxidom up work
oxidom status work
oxidom down work
```

Profiles run side by side, each on its own loopback address:

```sh
oxidom up work
oxidom up personal
oxidom list sessions
```

Ask for the right ports rather than assuming them:

```sh
eval "$(oxidom env work)"
```

## At boot

```sh
sudo systemctl enable --now oxidom.service   # the daemon
sudo systemctl enable --now oxidom@work      # bring `work` up at boot
```

On NixOS the template unit has no `[Install]` section, so enable an instance
declaratively:

```nix
services.oxidom.enable = true;
systemd.services."oxidom@work".wantedBy = [ "multi-user.target" ];
```

Note the system daemon keeps its database in `/var/lib/oxidom`, **not** in your
home directory — so subscriptions added by a session daemon are not visible to it.
See [the two databases](configuration.md#the-two-databases).

## Beyond one server

- **Pools** — select several servers and let the core balance across them, for
  spreading activity over exit addresses and surviving a dead node. In the GUI a
  saved filter *is* a pool. See [profiles-and-pools.md](profiles-and-pools.md#pools).
- **Per-app routing** — send one command through the tunnel and leave everything
  else alone:

  ```sh
  oxidom run --profile work -- curl https://ifconfig.me
  ```

  This needs a TUN interface on the profile; see [routing.md](routing.md).

## If something is wrong

```sh
oxidom status
journalctl -u oxidom.service -f
```

[troubleshooting.md](troubleshooting.md) is organised by symptom. The most common
surprise by far is [talking to a different daemon than last
time](troubleshooting.md#my-servers-vanished).

---

Next: [cli.md](cli.md) · [gui.md](gui.md) · [profiles-and-pools.md](profiles-and-pools.md)
