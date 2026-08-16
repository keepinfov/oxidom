# oxidom implementation spec

This directory is oxidom's binding implementation contract: the behaviour the code must keep,
written down together with the reasoning and the measurements that produced it. Where a page says
a thing is **binding**, changing it is a deliberate decision about the product, not a refactor —
and most of those lines exist because the opposite was tried and observed to fail, so the
reasoning is part of the contract and not commentary on it.

These pages are **normative**. They describe what the code does and must go on doing; they are not
a tutorial and not a changelog. User-facing documentation lives one level up in `../`.

Process rules — how to build, how to commit, what "done" means, the non-negotiable constraints and
the module layout — live in [`../../AGENTS.md`](../../AGENTS.md).

| Page | Governs |
|---|---|
| [storage.md](storage.md) | Config and state files on disk, which daemon owns the store, and the `config.toml` schema |
| [data-model.md](data-model.md) | The `Server`/`Subscription` types, subscription fetching, and the share-link parsers |
| [xray-config.md](xray-config.md) | The Xray JSON oxidom emits, pools and balancers, `[core]` settings, and the core supervisor |
| [latency.md](latency.md) | Probe methods, what a failed probe means, and the contract a measurement travels under |
| [cli.md](cli.md) | The `oxidom` command surface, output and exit codes, and handle/alias resolution |
| [profiles-and-pools.md](profiles-and-pools.md) | Profile files, pool selection and ranking, groups in the GUI, and running sessions |
| [interfaces.md](interfaces.md) | The privileged TUN path: device names, addresses, marks, routes and teardown |
| [gui.md](gui.md) | The window layout, the server browser, and the adaptive breakpoint |
