# cpc-breadcrumbs

Shared breadcrumb tracking library for CPC MCP servers. Provides crash-safe state for multi-step operations that span restarts, sessions, and agents.

**Part of [CPC](https://github.com/josephwander-arch) (Cognitive Performance Computing)** — a multi-agent AI orchestration system built on Claude + MCP (Model Context Protocol). Related repos: [manager](https://github.com/josephwander-arch/manager) · [local](https://github.com/josephwander-arch/local) · [hands](https://github.com/josephwander-arch/hands) · [workflow](https://github.com/josephwander-arch/workflow) · [cpc-paths](https://github.com/josephwander-arch/cpc-paths)

This crate is a dependency of CPC's MCP server binaries — most users won't install it directly; it's pulled in automatically via git tag when you build a CPC server from source.

## Install

Add to your `Cargo.toml`:

```toml
[dependencies]
cpc-breadcrumbs = { git = "https://github.com/josephwander-arch/cpc-breadcrumbs.git", tag = "v0.3.1" }
```

## What it does

Provides multi-step operation state, cross-session continuity, and fingerprint-based dedup for breadcrumb tracking across CPC servers. Consumed via git tag by CPC's Rust MCP servers.

Features:
- **One-file-per-breadcrumb** — each active breadcrumb is a standalone JSON file; no index to diverge
- **Atomic writes** — tmp+rename pattern prevents partial reads on concurrent access
- **Conflict detection** — fingerprint-based detection when multiple sessions write the same breadcrumb
- **Auto-reap** — configurable stale breadcrumb cleanup via `CPC_BREADCRUMB_AUTO_REAP_HOURS`
- **Drive-synced storage** — active and completed breadcrumbs live on Google Drive for Desktop (local-first, async cloud sync)
- **Legacy migration** — on first init(), reads old dual-store (index + JSONL) and migrates all breadcrumbs including orphans
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
Active:   C:\My Drive\Volumes\breadcrumbs\active\bc_{id}.json
Archive:  C:\My Drive\Volumes\breadcrumbs\completed\{YYYY-MM-DD}\bc_{id}.json
```

Default storage is under `C:\My Drive\Volumes\breadcrumbs\` on Windows with Google Drive for Desktop installed (`My Drive` is the local mount point for Google Drive). Override via the `CPC_VOLUMES_PATH` environment variable.

### Migration (v0.2 to v0.3)

On first `init()`, the library checks for legacy state at `C:\CPC\state\breadcrumbs\`. If found:
1. Reads all breadcrumbs from `active.index.json` and `projects/*.jsonl`
2. Writes each as an individual file in `active/` (including orphans that were unreachable in v0.2)
3. Renames the legacy directory to `breadcrumbs.migrated_{timestamp}` (reversible, not deleted)
4. Logs a summary to stderr

Migration is idempotent -- safe to run multiple times. Once the legacy dir is renamed, subsequent `init()` calls skip it.

## Environment variables

| Variable | Purpose |
|---|---|
| `CPC_ACTOR` | Actor name injected into writer context |
| `CPC_SESSION_ID` | Session ID for conflict detection |
| `CPC_BREADCRUMB_AUTO_REAP_HOURS` | Reap threshold in hours (default: 24). Set to `0` to disable. |

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

- v0.1.x -- Windows verified, file-locked JSONL storage, multi-project support
- v0.2.0 -- macOS/Linux verified, archive discipline, reconcile primitive
- v0.3.0 -- Unified one-file-per-breadcrumb storage, legacy migration, orphan recovery
- v0.3.1 -- Auto-reap stale breadcrumbs by default (24h threshold)

## Contributing

Issues welcome; PRs considered but this is primarily maintained as part of the CPC stack.

## License

Licensed under Apache-2.0 — see [LICENSE](LICENSE).
