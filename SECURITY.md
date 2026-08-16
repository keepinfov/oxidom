# Security policy

oxidom carries other people's traffic and holds the credentials that move it. A
defect here does not merely crash an application — it can expose which sites
someone visits, or the keys to their servers. Treat security reports
accordingly.

## Reporting a vulnerability

Report privately through
[GitHub's advisory form](https://github.com/keepinfov/oxidom/security/advisories/new).
Do not open a public issue, and do not describe the flaw in a pull request
before it is fixed.

Include what you did, what happened, and what you expected. A minimal
reproduction is worth more than a long description. If you have a patch, attach
it to the advisory rather than opening a PR.

Expect an acknowledgement within a week. Because this is a small project, a fix
may take longer than that; you will be told where it stands. Credit is given in
the advisory and the changelog unless you ask otherwise.

**Never paste a real subscription URL, share link, UUID, password, or server
address into an issue, a pull request, a log excerpt, or a screenshot** — those
are live credentials. Redact them, or use the invented values the test data
uses.

## What is in scope

- Traffic leaving the machine outside the tunnel while oxidom reports a
  connection: a routing rule that does not take effect, a system proxy left
  half-set, a TUN interface that fails open.
- Credentials reaching somewhere they should not: a share link written to a
  world-readable file, a UUID or password in a log line, a secret in a crash
  report or a D-Bus reply available to another user on the system.
- Anything that lets an unprivileged local user reach the system daemon's
  privileged operations, or that widens what the daemon does with
  `CAP_NET_ADMIN`.
- A subscription provider being able to identify a device that never opted in
  to sending a hardware id.
- A malicious subscription response reaching memory unsafety, a panic that takes
  the daemon down, or command execution while being parsed.

## What is not in scope

- Weaknesses in the Xray core itself — report those to
  [XTLS/Xray-core](https://github.com/XTLS/Xray-core).
- Weaknesses in a protocol oxidom merely speaks (for example, a censor
  fingerprinting a transport).
- A server operator seeing the traffic you send through their server. That is
  the trust model of a proxy, not a defect.
- Reports that require an attacker who already has root on the machine, or
  physical access to an unlocked session.

## Supported versions

Until `1.0.0`, only the current `master` receives fixes. There are no
maintained release branches yet; when tagged releases begin, this section will
say which of them are supported.

## What the project promises

These are invariants, not aspirations. If one of them is broken, that is a bug
worth reporting even without an exploit:

- The GUI and CLI run unprivileged. Only the system daemon is granted
  `CAP_NET_ADMIN`, and only for interface work.
- Configuration and state files are written `0600` through an atomic
  temp-and-rename, in a directory created `0700`.
- A hardware id is sent to a subscription provider only when that subscription
  has the option switched on. It is off by default.
- There is no telemetry, no analytics, and no phone-home of any kind.
- Secrets are not written to logs. The log ring buffer and any diagnostics are
  expected to be safe to paste into an issue; if you find one that is not, that
  is a bug.
