# Subscriptions and protocols

## Contents

- [Protocols](#protocols)
- [Share links](#share-links)
- [What subscriptions may return](#what-subscriptions-may-return)
- [The User-Agent decides the format](#the-user-agent-decides-the-format)
- [Quota and expiry](#quota-and-expiry)
- [Privacy and HWID](#privacy-and-hwid)
- [Refreshing](#refreshing)
- [What is deliberately not supported](#what-is-deliberately-not-supported)

## Protocols

| Protocol | Notes |
|---|---|
| **VLESS** | Reality, XTLS-Vision, xhttp/splithttp, WebSocket, gRPC, plain TCP |
| **VMess** | — |
| **Trojan** | TLS by default |
| **Shadowsocks** | SIP002, and the older fully-base64 form |
| **SOCKS** | with optional user/password |
| **HTTP** | — |
| **Hysteria2** | obfuscation and port hopping. **Needs Xray 26.1 or newer** — that is where the native `hysteria` outbound landed. An older core exits immediately instead of connecting. |

## Share links

Paste one link, or many at once — one per line. Base64-wrapped blobs of links are
detected and decoded.

| Scheme | Form |
|---|---|
| `vless://` | `uuid@host:port?type=…&security=…&sni=…&pbk=…&flow=…#name` |
| `vmess://` | base64 JSON |
| `trojan://` | `password@host:port?…#name` |
| `ss://` | SIP002, or legacy all-base64 |
| `socks://`, `socks5://` | optional `user:pass@` |
| `http://`, `https://` | default port 80 / 443 |
| `hysteria2://`, `hy2://` | default port 443; `hy2` is normalised so both spellings dedupe |

Hysteria2 port hopping is understood in both spellings — a suffix on the host
(`host:443,5000-6000,7000`) and the `mport`/`ports` query parameter.

Lines that cannot be parsed are skipped rather than failing the whole import, and
you are told how many and why. If nothing parses:

```
none of the links use a supported scheme
(vless, vmess, trojan, Shadowsocks, SOCKS, HTTP, Hysteria2)
```

Each server gets a **stable id** derived from the link, so refreshing a
subscription does not duplicate or re-shuffle anything. Give the ones you use a
readable handle with [`oxidom alias`](cli.md#oxidom-alias-handle-new).

## What subscriptions may return

oxidom accepts, in this detection order:

1. **A base64-encoded body** wrapping any of the below.
2. **A share-link list** — one link per line.
3. **JSON** — an array of Xray configs, Clash-in-JSON (`{proxies: …}`), or
   `{outbounds: […]}`, read as Xray if the outbounds carry a `protocol` field and
   as **sing-box** otherwise.
4. **YAML** — Clash.

An Xray config with more than one proxy outbound *and* a `routing.balancers`
section is imported as a **single server** representing the provider's own
balanced profile, labelled like `xray + balanced (12)`. Such a server cannot be a
member of an oxidom [pool](profiles-and-pools.md#pools) — it is already a
balancer.

If the panel sends something else, the error says which case you hit, and never
quotes the body back at you:

```
the panel sent a web page instead of a server list — it may not recognize
this app; try another Client preset in Settings
```

```
subscription returned no supported servers (expected a share-link list,
Xray or sing-box JSON, or Clash YAML)
```

The first one is common and usually means the User-Agent. Panels routinely gate
the response on it; change **Settings › Advanced › Client preset** (or
`subscription_user_agent`) and refresh.

## The User-Agent decides the format

The same URL commonly answers with a *different format* per client string, so
the User-Agent is not only about being recognized — it selects what you get.
A Remnawave panel, for example, may answer `v2rayNG` with an array of complete
Xray configs (one balanced profile per country) and answer `v2rayN` with a plain
share-link list of the same nodes. Both parse, but they are not equivalent:

| Response | What you see |
| --- | --- |
| Share-link list | one server per node, each with a share link, poolable, pingable |
| Array of balanced Xray configs | one `xray + balanced (N)` server per config, no share link, not poolable |

### What each panel answers with

Every panel oxidom claims to read has a case in the test suite named after it,
so an empty server list can be told apart from a shape nobody tried. The
fixtures live in `crates/oxidom-core/src/subscription_format/fixtures/`.

| Panel | Client preset | Format it answers with | Fixture |
| --- | --- | --- | --- |
| Marzban | v2rayN, v2rayNG | share-link list | `marzban-v2rayn.b64` |
| Marzban | Clash Meta | Clash YAML | `marzban-clash.yaml` |
| Marzban | sing-box, Hiddify | sing-box JSON | `marzban-sing-box.json` |
| Marzneshin | v2rayN, v2rayNG | share-link list | `marzneshin-v2rayn.b64` |
| Remnawave | v2rayN | share-link list | `remnawave-v2rayn.b64` |
| Remnawave | v2rayNG | array of whole Xray configs | `remnawave-v2rayng.json` |
| 3x-ui | v2rayN, v2rayNG | share-link list | `three-x-ui-v2rayn.b64` |
| Hiddify Manager | Hiddify, sing-box | sing-box JSON | `hiddify-manager-sing-box.json` |
| V2Board, XBoard | Clash Meta | Clash YAML | `v2board-clash.yaml` |
| any, unrecognised client | — | a web page, which is an error naming the cure | `panel-web-page.html` |

**None of these has been tried against a live panel.** No instance of any of
them was available, so each fixture is written from the format that panel is
documented to serve for that client string, with invented credentials
throughout — the table says what oxidom parses, not what a particular
installation was observed to send. A panel that answers differently from its
row is worth an issue: that is the gap these cases exist to expose.

A share-link list normally arrives base64-encoded, which oxidom unwraps before
parsing; the `.b64` fixtures are stored that way because the wrapper is a case
of its own, and `base64 -d` reads one.

So a subscription that shows far fewer normal servers than you expect — most of
them labelled `xray + balanced (…)` — is usually answering the wrong format
rather than hiding nodes. Try a different client string and refresh.

Because providers disagree about which client gets what, the value is settable
**per subscription**: open the subscription and use **Fetching › User-Agent
override**. Leave it empty to inherit **Settings › Advanced › Client preset**.
The new value applies on the next update, so press **Update** afterwards.

## Quota and expiry

If the response carries a `subscription-userinfo` header, oxidom reads upload,
download, total and expiry from it and shows them on the subscription. A
`profile-title` header (optionally base64) names the subscription, and
`profile-update-interval` is read as the provider's suggested refresh cadence.

## Privacy and HWID

- **No telemetry.** Nothing is reported anywhere.
- Subscription URLs are the access token, so they are **never** included in error
  messages or in `--json` output.
- Every state file is written `0600`.

Some providers limit a subscription to a number of devices and want a hardware id.
oxidom will send one — **only if you turn it on for that subscription**, and never
by default. The per-install identifier is not even generated until something opts
in.

When enabled, the request carries `x-hwid`, plus `x-device-os: Linux`,
`x-ver-os: <arch>` and `x-device-model: oxidom`. When not enabled, nothing
identifying is sent beyond the User-Agent you chose.

A provider that requires it says so, and oxidom passes that along:

```
this subscription requires a device identifier;
enable Advanced > Send HWID and add it again
```

## Refreshing

A refresh matches servers first by id and then by connection details, so a node
renamed upstream keeps its alias, its id, and its last latency rather than
appearing as a new server.

Two consequences worth knowing:

- If a refresh **drops the server a session is using**, that session is
  disconnected, with the note `the active server is no longer in its subscription
  — disconnected`. It is not silently repointed at something you did not choose.
- Removing a subscription or a server disconnects any session carrying it.

Pools defined as [rules](profiles-and-pools.md#lists-and-rules) pick up new
servers on their own; pools defined as lists do not.

## What is deliberately not supported

| | Why |
|---|---|
| `hysteria://` (v1) | A different wire protocol from Hysteria2, not a variant of it. |
| `tuic://` | No Xray outbound for it. |
| `ssh://` | Out of scope. |
| Shadowsocks SIP003 plugins (`plugin=`) | Xray cannot run them. The link imports, with a warning that the server will likely not work. |

Unsupported schemes in an import are **counted and reported**, so you are told
that eight of forty links were dropped rather than silently losing them.

One more thing worth knowing, since links in the wild still carry it:
`allowInsecure` no longer does anything — Xray 26.x removed it. A link asking to
skip certificate verification gets a warning, and the certificate is verified
normally. The only supported escape hatch is a certificate pin (`pinSHA256`).

---

Next: [configuration.md](configuration.md) · [profiles-and-pools.md](profiles-and-pools.md) · [troubleshooting.md](troubleshooting.md)
