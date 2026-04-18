# Changelog

## [Unreleased]

### Added

- **GitHub Actions CI workflow** — push/PR to main runs mojibake scan, cargo check (x64 + ARM64), fmt, clippy, version alignment.
- **GitHub Actions release workflow** — `v*` tag push validates library builds on both targets.
- **SECURITY.md** — security policy and reporting instructions.

## [0.2.0] - 2026-04-17

### Changed
- **D2: Display source separation** — `list(scope)` now defaults to `"active"`, reading ONLY `active.index.json`. Project JSONL files are no longer consulted as a display source (they remain append-only audit logs). Use `scope="today"`, `"week"`, or `"all"` to include completed archive entries.
- **D2: status() simplified** — `status()` now reads ONLY `active.index.json` (removed `scope` param; use `list()` for historical queries). No longer loads full breadcrumb records from project JSONL.
- **Storage path override** — `state_dir()` now checks `CPC_BREADCRUMB_STATE_DIR` env var before defaulting to `C:\CPC\state\breadcrumbs`. Archive and handoff dirs similarly configurable via `CPC_BREADCRUMB_ARCHIVE_DIR` and `CPC_BREADCRUMB_HANDOFF_DIR`.

### Added
- **D1: Archive discipline** — `abort()` archives to `completed/YYYY-MM-DD/{id}.json` with `aborted: true` and `abort_reason` fields, distinguishable from completions (which have `aborted: false`).
- **D3: Graceful abort/adopt fallback** — `abort(id)` on an already-archived breadcrumb returns `{"status": "already_archived", ...}` with archive details instead of a bare "not found" error. `adopt(id)` on an archived breadcrumb returns a descriptive error with date, result type, and archive path.
- **D3: Archive search** — New internal `archive::find_archived(id, days_back)` searches completed archives for a breadcrumb by ID across recent date folders.
- **D4: Reconcile primitive** — New public `reconcile(stale_threshold_hours) -> ReconcileReport` function. Scans `active.index.json`, identifies entries older than the threshold that haven't been updated in 30+ minutes, removes them from the active index, and writes a markdown handoff file to `Volumes/handoffs/pending_breadcrumbs_YYYY-MM-DD.md`.
- **D4: New public types** — `StaleEntry { id, name, last_activity_at, hours_stale, project_id }` and `ReconcileReport { scanned, stale_found, handoff_path, handoff_entries_written }`.
- **Tests** — 4 new tempdir-isolated unit tests covering D1 (abort vs complete archive flags), D2 (active scope reads only index), D3 (abort/adopt fallback on archived IDs), D4 (reconcile stale detection and handoff file generation).

## [0.1.0] - 2026-04-16

### Added
- Initial public release — shared breadcrumb tracking crate for CPC servers.
- Multi-project concurrent breadcrumbs with per-project file locking.
- Conflict detection, Drive-synced archiving, backward-compatible single-slot semantics.
