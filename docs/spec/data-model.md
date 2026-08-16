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

1. HTTP GET the subscription URL with `ureq`. Send a normal browser-ish `User-Agent`.
   If `send_hwid` is true for that sub, add the HWID header (Happ uses an `x-hwid`-style header —
   send `Hwid: <id>` and `User-Agent` including the app; **only when opted in**). Otherwise send
   nothing identifying.
2. Read response headers: `subscription-userinfo` (`upload=..; download=..; total=..; expire=..`),
   `profile-title` (may be base64 with `base64:` prefix), `profile-update-interval`.
3. Body may be base64-encoded; if it decodes to text lines, use that, else use raw text.
4. Split into lines; parse each non-empty line as a share link (below). Skip unparseable lines.

### Share-link parsers → `Server`

- `vless://uuid@host:port?<params>#name` — params: `type` (tcp/ws/grpc/xhttp/splithttp),
  `security` (none/tls/reality), `sni`, `pbk`, `sid`, `fp`, `flow` (xtls-rprx-vision),
  `path`, `host`, `serviceName`, `alpn`, `encryption`. Build `transport_label` like
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

Derive `country` from a leading flag emoji or country code in the name when present.
