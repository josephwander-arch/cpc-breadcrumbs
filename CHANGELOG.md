# Changelog

## [Unreleased]

## [0.3.0] - 2026-04-18

### Changed
- **Unified storage** -- replaced dual-store (active.index.json + projects/*.jsonl) with one-file-per-breadcrumb in `Volumes/breadcrumbs/active/bc_{id}.json`. Eliminates the index-vs-project-file split-brain that caused count mismatches and unreachable orphans.
- **Atomic writes** -- all breadcrumb writes use tmp+rename pattern. No file locking needed.
- **Archive path** -- `complete()` and `abort()` now rename from `active/` to `completed/{date}/` (same volume, atomic).
- **Removed `fs2` dependency** -- no more per-project file locking.
- **Removed `ProjectLocked` error variant** -- no longer applicable.

### Added
- **Legacy migration** -- `init()` calls `migrate_legacy()` on startup. Reads old index + JSONL files, writes each breadcrumb as an individual JSON file. Orphans (in JSONL but missing from index) are migrated too. Legacy directory is renamed to `breadcrumbs.migrated_{timestamp}` (reversible).
- **`mutate_breadcrumb()`** -- internal read-modify-write helper with atomic persistence.
- **13 unit tests** -- migration with orphans, orphan abort recovery, atomic write verification, concurrent starts (10 threads), conflict detection, stale detection, start/step/complete/abort/adopt lifecycle.

### Fixed
- **Count mismatch** -- `active_count()` and `status()` now read the same source (directory listing), eliminating the 2-vs-4 divergence.
- **Abort "not found" on orphans** -- orphaned breadcrumbs (present in JSONL but absent from index) are now first-class citizens in the new storage and fully reachable by `abort()`, `adopt()`, and all mutation paths.
- **Stale persistence across restarts** -- orphans no longer survive indefinitely; they are either migrated (if legacy) or reapable (if active).

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
