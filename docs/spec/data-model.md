# Data model and subscriptions

The types oxidom stores, and how a subscription body becomes them.

## Data model

```rust
enum Protocol { Vless, Vmess, Trojan, Shadowsocks, Socks, Http, Hysteria2 }

struct Server {
    id: String,            // stable hash of the link
    name: String,          // remark / tag
    protocol: Protocol,
    address: String,
    port: u16,
    // Transport/security summary for the card subtitle, e.g. "vless + xhttp + reality".
    transport_label: String,
    country: Option<String>, // ISO code, for the flag; parsed from name if present
    raw: OutboundSpec,       // everything needed to emit Xray outbound JSON
    // RFC 7396 fragment merged onto the generated outbound; absent unless a
    // hand-entered server carried one. See "A server entered as fields".
    outbound_patch: Option<serde_json::Value>,
    latency_ms: Option<u32>, // last probe result (runtime only)
}

struct Subscription {
    id: String,
    name: String,             // from profile-title header, else user-given
    url: String,
    description: Option<String>,
    userinfo: Option<UserInfo>, // upload/download/total/expire
    send_hwid: bool,            // OPT-IN, default false
    servers: Vec<Server>,
    updated_at: Option<i64>,
}

struct UserInfo { upload: u64, download: u64, total: u64, expire: Option<i64> }
```

## Subscription fetch & parse

1. HTTP GET the subscription URL with `ureq`. Send a **client** `User-Agent`, not a browser one:
   panels gate both the body and its format on it, so the string is a choice from
   `subscription::CLIENT_PRESETS` — defaulting to `v2rayN/6.45` — settable per subscription over
   the global one. Which panel answers with which format is tabulated in
   [subscriptions-and-protocols.md](../subscriptions-and-protocols.md#what-each-panel-answers-with),
   where every case has a fixture named after the panel.
   If `send_hwid` is true for that sub, add the Happ/Remnawave device headers — `x-hwid` carries
   the identifier, and `x-device-os`, `x-ver-os` and `x-device-model` let the panel label it —
   **only when opted in**. Otherwise send nothing identifying.
2. Read response headers: `subscription-userinfo` (`upload=..; download=..; total=..; expire=..`),
   `profile-title` (may be base64 with `base64:` prefix), `profile-update-interval`.
3. Body may be base64-encoded; if it decodes to text lines, use that, else use raw text.
4. Split into lines; parse each non-empty line as a share link (below). Skip unparseable lines.

### A server entered as fields (binding)

A server can enter the store without a link: a `ServerDraft` — name, protocol, address, port,
the per-protocol credential (`uuid`, `password`, `method`+`password`, `auth`), an optional
`stream: StreamSettings` or `hysteria2: Hysteria2Settings` block, and an optional
`outbound_patch` — travels to the daemon as JSON over `CreateServer(s draft_json) → s`
(answering `ipc::CreatedServer`: id, name, assigned alias). Field names are the model's serde
names deliberately, so the JSON key a dialog labels is the key that reaches the stored server.

- **Validation ends by generating.** `draft::resolve` builds the server and proves it against
  `xray::config::outbound_tagged` — the one path every connect uses — with the patch merged.
  A draft that fails names the field that stops it, and nothing is created.
- **`outbound_patch` is the escape hatch** for a core option no field models: a JSON object
  merged onto the generated outbound RFC 7396 style (objects merge, `null` removes, anything
  else replaces), carried on the server so generation reproduces it forever. It may not set
  `tag` or `protocol` — those belong to the protocol choice and the generator.
- **The id is assigned when a server enters the store and never recomputed.** For link-imported
  servers the identity is the link; for hand-made ones it is the serialized spec, patch
  included — two drafts differing only in their patch are two servers. Editing later must
  address by id, not re-derive it.
- **A hand-made server lives in the local `"local"` group** — the same place pasted links
  live — whose empty URL every refresh path filters out, so no refresh can overwrite or drop
  it. A draft identical to a stored local server is refused loudly, naming that server; the
  silent skip a pasted batch gets would hide the typo that made them identical.

### Editing a stored server (binding)

`UpdateServer(s server_id, s draft_json) → s` (answering `ipc::CreatedServer`) edits a stored
server through the same `draft::resolve` validator `CreateServer` ends in, and **the id never
changes** — profiles, aliases and unit names hang off it, and a refresh still matches the entry
by that id.

- **The stored entry is the effective server**: the provider's fields with the user's decisions
  applied. `draft_from_server` reads it back as the draft the dialog prefills with, and an edit
  is `diff` between that prefill and the dialog's output.
- **A subscription's server records edits as overrides** — `ServerOverrides { values, provider }`
  keyed by draft field names, the nested blocks dotted (`stream.sni`, `hysteria2.up_mbps`):
  `values` holds the user's decisions, `provider` what the provider last sent for those same
  keys, so a drop needs no refresh. A hand-made server in the local group is simply rewritten;
  it has no provider to override.
- **A subscription's server cannot change protocol.** The fields under another protocol would
  not be the same server with a typo fixed; adding a server is the honest route.
- **Every refresh puts the values back on top.** `preserve_server_identity` carries `overrides`
  across like the alias, restamps `provider` from the entry as the refresh brought it (so a
  drop falls back to the newest provider value), and applies `values` through
  `draft::apply_overrides` — the same validator again, with the untouched nested blocks riding
  along whole.
- **Matching sees through the overrides.** The stored entry carries the user's port; the fresh
  one does not, so they never describe the same connection under `same_connection_as`. A third
  matcher compares the provider base against the fresh entry and accepts when everything that
  drifted is the display name or an overridden field. Id and strict matchers run first, and the
  one-pass `used` guard bounds the ambiguity.
- **Editing back to the provider's own value clears the override** rather than recording a
  decision that matches the default, and `DropOverride(s server_id, s field) → s` takes the
  provider's value back for one field, leaving the other decisions alone.
- **An edit does not restart a connected profile.** The next connect uses the edited fields.

### Share-link parsers → `Server`

- `vless://uuid@host:port?<params>#name` — params: `type` (tcp/ws/grpc/xhttp/splithttp),
  `security` (none/tls/reality), `sni`, `pbk`, `sid`, `fp`, `flow` (xtls-rprx-vision),
  `path`, `host`, `serviceName`, `authority`, `mode`, `extra`, `alpn`, `encryption`. XHTTP preserves
  its `mode` (`auto`, `packet-up`, `stream-up`, `stream-one`) and object-valued `extra` settings
  such as XMUx and padding; gRPC preserves `authority` and `mode=multi`, while a leading slash in
  `serviceName` is removed only when generating Xray JSON.
  Build `transport_label` like
  `"vless + xhttp + reality"`.
- `vmess://<base64 json>` — JSON with `add/port/id/aid/net/tls/host/path/sni/scy/ps`.
- `trojan://password@host:port?<params>#name` — tls params like vless.
- `ss://` — SIP002: `ss://base64(method:password)@host:port#name` or fully-base64 form.
- `socks://` / `http://` — optional userinfo auth.
- `hysteria2://` (alias `hy2://`) — `auth@host:port[,ranges]?<params>#name`. Params: `obfs`
  (only `salamander`), `obfs-password`, `sni`, `insecure`, `pinSHA256`, `alpn`, `up`, `down`,
  `hopInterval`, `congestion`. The port defaults to 443, the auth string is opaque and may
  contain `:`, and the comma-separated port-hopping ranges must come off the authority before
  `Url::parse` will accept the link. Settings live in `Hysteria2Settings`, not `StreamSettings`.
  Bare `hysteria://` is **v1** and stays unsupported.

`StreamSettings.pin_sha256` is a **local mark**, not provider data (binding): it is set only by
a user trusting a certificate, it is carried across a refresh the way an alias is, and
`same_connection_as` ignores it when matching a refreshed entry to the one it replaces. Comparing
it would mean a server stopped matching its own refreshed entry the moment someone trusted its
certificate, losing the alias and the stable id along with the pin. A pin the provider itself
sends is not overwritten by a carried one.

Derive `country` from a leading flag emoji, or from a leading two-letter token that is an
assigned ISO 3166-1 alpha-2 code — `🇩🇪 Frankfurt` and `DE-2 HYSTERIA2` both give `DE`. Only the
**first** token counts, and only a real code (binding): `IS`, `IT`, `NO`, `ME`, `AT` and `WS` are
countries *and* ordinary words, so matching one anywhere in a name would put wrong flags across
half a provider's list. A name that says nothing stays `None`; nothing is inferred from the
address, so a provider whose names carry no country shows no flags.
