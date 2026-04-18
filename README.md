# cpc-breadcrumbs

Shared breadcrumb tracking library for CPC MCP servers. Provides crash-safe state for multi-step operations that span restarts, sessions, and agents.

**Part of [CPC](https://github.com/josephwander-arch) (Cognitive Performance Computing)** — a multi-agent AI orchestration system built on Claude + MCP (Model Context Protocol). Related repos: [manager](https://github.com/josephwander-arch/manager) · [local](https://github.com/josephwander-arch/local) · [hands](https://github.com/josephwander-arch/hands) · [workflow](https://github.com/josephwander-arch/workflow) · [cpc-paths](https://github.com/josephwander-arch/cpc-paths)

This crate is a dependency of CPC's MCP server binaries — most users won't install it directly; it's pulled in automatically via git tag when you build a CPC server from source.

## Install

Add to your `Cargo.toml`:

```toml
[dependencies]
cpc-breadcrumbs = { git = "https://github.com/josephwander-arch/cpc-breadcrumbs.git", tag = "v0.2.0" }
```

## What it does

Provides multi-step operation state, cross-session continuity, and fingerprint-based dedup for breadcrumb tracking across CPC servers. Consumed via git tag by CPC's Rust MCP servers.

Features:
- **Multi-project support** — per-project JSONL storage with file-level locking
- **Fingerprint dedup** — conflict detection when multiple sessions write the same breadcrumb
- **Auto-reap** — configurable stale breadcrumb cleanup via `CPC_BREADCRUMB_AUTO_REAP_HOURS`
- **Drive-synced archiving** — completed breadcrumbs archived to `C:\My Drive\Volumes\breadcrumbs\completed\{YYYY-MM-DD}\`
- **Backward compatibility** — callers without `project_id` use `_ungrouped`; callers without `breadcrumb_id` work as long as exactly one breadcrumb is active

### Design Principles (D1–D4)

- **D1 — Archive discipline**: completed breadcrumbs are always archived, never silently dropped
- **D2 — Display-source separation**: the storage format is independent of how breadcrumbs are rendered
- **D3 — Graceful abort/adopt**: orphaned breadcrumbs from crashed sessions can be adopted by new sessions or explicitly aborted
- **D4 — Reconcile primitive**: concurrent writers are detected via fingerprints and resolved deterministically

## Usage

```rust
use cpc_breadcrumbs::{WriterContext, start, step, complete};

fn main() -> anyhow::Result<()> {
    cpc_breadcrumbs::init();

    let ctx = WriterContext::from_env();

    let resp = start(
        "my operation | targets: src/main.rs",
        vec!["step 1".into(), "step 2".into()],
        Some("my_project".into()),
        &ctx,
    )?;

    step("step 1 done", vec!["src/main.rs".into()], None, &ctx)?;
    complete("all done", None, &ctx)?;

    Ok(())
}
```

## Storage layout

```
Active:   C:\CPC\state\breadcrumbs\active.index.json
          C:\CPC\state\breadcrumbs\projects\{project_id}.jsonl
Archive:  C:\My Drive\Volumes\breadcrumbs\completed\{YYYY-MM-DD}\bc_{id}.json
```

## Environment variables

| Variable | Purpose |
|---|---|
| `CPC_ACTOR` | Actor name injected into writer context |
| `CPC_SESSION_ID` | Session ID for conflict detection |
| `CPC_BREADCRUMB_AUTO_REAP_HOURS` | Reap stale breadcrumbs older than N hours (0 = disabled) |

## Build from Source

```bash
git clone https://github.com/josephwander-arch/cpc-breadcrumbs.git
cd cpc-breadcrumbs
cargo build
```

This is a library crate — no binary is produced. Requires Rust stable toolchain.

## Requirements

- Rust stable toolchain
- Windows 10/11 (primary), macOS/Linux (v0.2.0+)

## Versioning

- v0.1.x — Windows verified, file-locked JSONL storage, multi-project support
- v0.2.0 — macOS/Linux verified

## Contributing

Issues welcome; PRs considered but this is primarily maintained as part of the CPC stack.

## License

Licensed under Apache-2.0 — see [LICENSE](LICENSE).
