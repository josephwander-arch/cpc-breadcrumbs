# cpc-breadcrumbs

Shared breadcrumb tracking library for CPC MCP servers.

## What it does

Provides multi-step operation state, cross-session continuity, and fingerprint-based dedup for breadcrumb tracking across CPC servers. Used by `autonomous` and `local` servers.

Features:
- **Multi-project support** — per-project JSONL storage with file-level locking
- **Fingerprint dedup** — conflict detection when multiple sessions write the same breadcrumb
- **Auto-reap** — configurable stale breadcrumb cleanup via `CPC_BREADCRUMB_AUTO_REAP_HOURS`
- **Drive-synced archiving** — completed breadcrumbs archived to `C:\My Drive\Volumes\breadcrumbs\completed\{YYYY-MM-DD}\`
- **Backward compatibility** — callers without `project_id` use `_ungrouped`; callers without `breadcrumb_id` work as long as exactly one breadcrumb is active

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

## Part of CPC

`cpc-breadcrumbs` is part of [CPC (Cognitive Performance Computing)](https://github.com/josephwander-arch), a multi-agent AI orchestration platform with 460+ tools across 13 MCP servers.

Related crates:
- [`cpc-paths`](https://github.com/josephwander-arch/cpc-paths) — portable path discovery for CPC MCP servers

## Versioning

- v0.1.x — Windows verified, file-locked JSONL storage, multi-project support
- v0.2.0 — macOS/Linux verified

## License

Apache 2.0
