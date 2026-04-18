//! cpc-breadcrumbs — shared breadcrumb tracking for CPC MCP servers.
//!
//! Provides multi-project concurrent breadcrumbs with per-project file locking,
//! conflict detection, Drive-synced archiving, and backward-compatible single-slot semantics.
//!
//! # Storage layout
//! Active:  `C:\CPC\state\breadcrumbs\active.index.json` + `projects\{pid}.jsonl`
//! Archive: `C:\My Drive\Volumes\breadcrumbs\completed\{YYYY-MM-DD}\bc_{id}.json`
//!
//! # Backward compatibility
//! Callers that pass no `project_id` get project `_ungrouped`.
//! Callers that pass no `breadcrumb_id` work as long as there is exactly one active breadcrumb.

mod archive;
mod conflict;
pub mod error;
pub mod schema;
mod storage;

pub use error::BreadcrumbError;
pub use schema::{Breadcrumb, ConflictInfo, IndexEntry};
// Re-export reconcile types (D4)
// StaleEntry and ReconcileReport are defined below in this file

use serde::Serialize;

use serde_json::{json, Value};
use storage::{
    ensure_dirs, index_remove, index_upsert, load_all_active, load_project, locked_write_project,
    read_index, resolve,
};

// ── Writer context ─────────────────────────────────────────────────────────────

/// Caller identity passed into every write operation.
#[derive(Debug, Clone, Default)]
pub struct WriterContext {
    pub actor: String,
    pub machine: String,
    pub session: String,
}

impl WriterContext {
    pub fn new(
        actor: impl Into<String>,
        machine: impl Into<String>,
        session: impl Into<String>,
    ) -> Self {
        WriterContext {
            actor: actor.into(),
            machine: machine.into(),
            session: session.into(),
        }
    }

    /// Build from environment — used by servers that don't inject identity.
    pub fn from_env() -> Self {
        WriterContext {
            actor: std::env::var("CPC_ACTOR").unwrap_or_else(|_| "unknown".to_string()),
            machine: machine_name(),
            session: std::env::var("CPC_SESSION_ID").unwrap_or_else(|_| "session_0".to_string()),
        }
    }
}

// ── Machine name detection ─────────────────────────────────────────────────────

/// Resolve hostname using env vars with syscall fallback.
/// Priority: COMPUTERNAME → HOSTNAME → hostname::get() → "unknown"
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        })
        .to_lowercase()
}

// ── Server init (call from main) ───────────────────────────────────────────────

/// Must be called on server startup. Creates storage dirs and optionally reaps stale breadcrumbs.
/// Checks `CPC_BREADCRUMB_AUTO_REAP_HOURS` env var:
///   - unset / empty / "0" / invalid → auto-reap disabled
///   - positive integer N           → reap breadcrumbs with last_activity_at > N hours ago
pub fn init() {
    if let Err(e) = ensure_dirs() {
        eprintln!("[cpc-breadcrumbs] Failed to create state dirs: {}", e);
    }

    let hours = std::env::var("CPC_BREADCRUMB_AUTO_REAP_HOURS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&h| h > 0);

    if let Some(h) = hours {
        storage::reap_stale(h);
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

pub fn has_active() -> bool {
    !read_index().is_empty()
}

pub fn active_count() -> usize {
    read_index().len()
}

/// Return a snapshot of all currently active breadcrumbs across all projects.
pub fn list_active() -> Vec<Breadcrumb> {
    load_all_active()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ── start ──────────────────────────────────────────────────────────────────────

/// Start a new breadcrumb.
///
/// `project_id = None` → stored under `_ungrouped`.
/// Returns the new breadcrumb ID and project_id in the JSON response.
pub fn start(
    name: &str,
    steps: Vec<String>,
    project_id: Option<String>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    ensure_dirs()?;

    let id = schema::new_id(name);
    let pid = project_id
        .clone()
        .unwrap_or_else(|| "_ungrouped".to_string());
    let now = now_rfc3339();
    let total = steps.len();

    let bc = Breadcrumb {
        id: id.clone(),
        name: name.to_string(),
        project_id: project_id.clone(),
        owner: ctx.actor.clone(),
        writer_actor: ctx.actor.clone(),
        writer_machine: ctx.machine.clone(),
        writer_session: ctx.session.clone(),
        writer_at: now.clone(),
        started_at: now.clone(),
        last_activity_at: now.clone(),
        steps: steps.clone(),
        current_step: 0,
        total_steps: total,
        step_results: Vec::new(),
        files_changed: Vec::new(),
        stale: false,
        conflict_warning: None,
        aborted: false,
        abort_reason: None,
        auto_started: false,
    };

    // Write to project file
    locked_write_project(&pid, |bcs| {
        bcs.push(bc.clone());
        Ok(())
    })?;

    // Update index
    index_upsert(IndexEntry {
        id: id.clone(),
        project_id: project_id.clone(),
        name: name.to_string(),
        owner: ctx.actor.clone(),
        last_activity_at: now.clone(),
        started_at: now,
    })?;

    Ok(json!({
        "status": "started",
        "id": id,
        "name": name,
        "project_id": pid,
        "steps": steps,
        "total_steps": total
    }))
}

// ── step ───────────────────────────────────────────────────────────────────────

/// Record completion of a step and advance to the next.
///
/// `breadcrumb_id = None` → infers from active index (error if 0 or >1 active).
pub fn step(
    result: &str,
    files_changed: Vec<String>,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let (bc_id, pid) = resolve(breadcrumb_id)?;
    let now = now_rfc3339();
    let mut out_step_name = String::new();
    let mut out_current = 0usize;
    let mut out_total = 0usize;
    let mut conflict: Option<ConflictInfo> = None;

    locked_write_project(&pid, |bcs| {
        let bc = bcs
            .iter_mut()
            .find(|b| b.id == bc_id)
            .ok_or_else(|| BreadcrumbError::NotFound { id: bc_id.clone() })?;

        // Conflict detection
        conflict = conflict::check(bc, &ctx.session);

        let step_name = bc
            .steps
            .get(bc.current_step)
            .cloned()
            .unwrap_or_else(|| format!("step_{}", bc.current_step + 1));

        let step_idx = bc.current_step;
        bc.step_results.push(schema::StepResult {
            step_idx,
            step_name: step_name.clone(),
            result: result.to_string(),
            at: now.clone(),
            files_changed: files_changed.clone(),
        });
        bc.files_changed.extend(files_changed.iter().cloned());
        bc.current_step += 1;
        bc.last_activity_at = now.clone();
        bc.writer_actor = ctx.actor.clone();
        bc.writer_machine = ctx.machine.clone();
        bc.writer_session = ctx.session.clone();
        bc.writer_at = now.clone();
        if let Some(ref c) = conflict {
            bc.conflict_warning = Some(c.clone());
        }

        out_step_name = step_name;
        out_current = bc.current_step;
        out_total = bc.total_steps;

        Ok(())
    })?;

    // Update index last_activity_at
    let mut index = read_index();
    if let Some(entry) = index.get_mut(&bc_id) {
        entry.last_activity_at = now;
    }
    write_index_silent(&index);

    let remaining = out_total.saturating_sub(out_current);

    let mut resp = json!({
        "step_completed": out_step_name,
        "current": out_current,
        "total": out_total,
        "remaining": remaining,
        "breadcrumb_id": bc_id
    });

    if let Some(c) = conflict {
        resp["conflict_warning"] = serde_json::to_value(&c).unwrap_or(Value::Null);
    }

    Ok(resp)
}

// ── complete ───────────────────────────────────────────────────────────────────

/// Mark a breadcrumb complete and archive it to Drive.
pub fn complete(
    summary: &str,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let (bc_id, pid) = resolve(breadcrumb_id)?;
    let now = now_rfc3339();
    let mut archived_path = String::new();
    let mut bc_name = String::new();
    let mut files_changed: Vec<String> = Vec::new();
    let mut steps_completed = 0usize;

    locked_write_project(&pid, |bcs| {
        let pos = bcs
            .iter()
            .position(|b| b.id == bc_id)
            .ok_or_else(|| BreadcrumbError::NotFound { id: bc_id.clone() })?;

        let mut bc = bcs.remove(pos);
        bc.last_activity_at = now.clone();
        bc.writer_actor = ctx.actor.clone();
        bc.writer_machine = ctx.machine.clone();
        bc.writer_session = ctx.session.clone();
        bc.writer_at = now.clone();
        bc.stale = false;

        bc_name = bc.name.clone();
        files_changed = bc.files_changed.clone();
        steps_completed = bc.step_results.len();

        // Archive to Drive
        let path = archive::archive(&bc).unwrap_or_else(|_| std::path::PathBuf::new());
        archived_path = path.to_string_lossy().to_string();

        Ok(())
    })?;

    // Remove from index
    index_remove(&bc_id)?;

    Ok(json!({
        "status": "completed",
        "id": bc_id,
        "name": bc_name,
        "steps_completed": steps_completed,
        "files_changed": files_changed,
        "summary": summary,
        "archived_to": archived_path,
        "EXTRACT_NOW": true,
        "note": "Review work for extraction-worthy insights (3Q gate: Reusable? Specific? New?)"
    }))
}

/// Like `start` but with additional options (auto_started flag, etc.).
/// Used by server-internal auto-tracking (e.g. local auto-breadcrumb).
pub fn start_auto(
    name: &str,
    steps: Vec<String>,
    project_id: Option<String>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    ensure_dirs()?;

    let id = schema::new_id(name);
    let pid = project_id
        .clone()
        .unwrap_or_else(|| "_ungrouped".to_string());
    let now = now_rfc3339();
    let total = steps.len();

    let bc = Breadcrumb {
        id: id.clone(),
        name: name.to_string(),
        project_id: project_id.clone(),
        owner: ctx.actor.clone(),
        writer_actor: ctx.actor.clone(),
        writer_machine: ctx.machine.clone(),
        writer_session: ctx.session.clone(),
        writer_at: now.clone(),
        started_at: now.clone(),
        last_activity_at: now.clone(),
        steps: steps.clone(),
        current_step: 0,
        total_steps: total,
        step_results: Vec::new(),
        files_changed: Vec::new(),
        stale: false,
        conflict_warning: None,
        aborted: false,
        abort_reason: None,
        auto_started: true,
    };

    locked_write_project(&pid, |bcs| {
        bcs.push(bc.clone());
        Ok(())
    })?;

    index_upsert(IndexEntry {
        id: id.clone(),
        project_id: project_id.clone(),
        name: name.to_string(),
        owner: ctx.actor.clone(),
        last_activity_at: now.clone(),
        started_at: now,
    })?;

    Ok(json!({
        "status": "started",
        "id": id,
        "name": name,
        "project_id": pid,
        "steps": steps,
        "total_steps": total,
        "auto_started": true
    }))
}

// ── abort ──────────────────────────────────────────────────────────────────────

/// Abort a breadcrumb with a reason and archive it.
///
/// If the breadcrumb is not in the active index, checks the completed archive
/// (today + 7 days). If found there, returns success with `status: "already_archived"`.
pub fn abort(
    reason: &str,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let resolve_result = resolve(breadcrumb_id);

    // D3: If not found in active index and we have an explicit ID, check archive
    if let Err(BreadcrumbError::NotFound { ref id }) = resolve_result {
        if let Some((bc, path)) = archive::find_archived(id, 7) {
            return Ok(json!({
                "status": "already_archived",
                "id": bc.id,
                "name": bc.name,
                "archived_at": bc.last_activity_at,
                "result_type": if bc.aborted { "aborted" } else { "completed" },
                "reason": bc.abort_reason,
                "archived_path": path.to_string_lossy()
            }));
        }
    }

    let (bc_id, pid) = resolve_result?;
    let now = now_rfc3339();
    let mut bc_name = String::new();
    let mut steps_completed = 0usize;

    locked_write_project(&pid, |bcs| {
        let pos = bcs
            .iter()
            .position(|b| b.id == bc_id)
            .ok_or_else(|| BreadcrumbError::NotFound { id: bc_id.clone() })?;

        let mut bc = bcs.remove(pos);
        bc.aborted = true;
        bc.abort_reason = Some(reason.to_string());
        bc.last_activity_at = now.clone();
        bc.writer_actor = ctx.actor.clone();
        bc.writer_session = ctx.session.clone();
        bc.writer_at = now.clone();

        bc_name = bc.name.clone();
        steps_completed = bc.step_results.len();

        // Archive to Drive
        let _ = archive::archive(&bc);
        Ok(())
    })?;

    index_remove(&bc_id)?;

    Ok(json!({
        "status": "aborted",
        "id": bc_id,
        "name": bc_name,
        "reason": reason,
        "steps_completed": steps_completed
    }))
}

// ── status ─────────────────────────────────────────────────────────────────────

/// Get status of active breadcrumbs. Reads ONLY `active.index.json`.
///
/// `project_id`: filter to a specific project.
pub fn status(project_id: Option<&str>) -> Result<Value, BreadcrumbError> {
    let index = read_index();

    let entries: Vec<&IndexEntry> = if let Some(pid) = project_id {
        index
            .values()
            .filter(|e| e.project_id.as_deref() == Some(pid))
            .collect()
    } else {
        index.values().collect()
    };

    if entries.is_empty() {
        return Ok(json!({ "active": false, "breadcrumbs": [] }));
    }

    let summaries: Vec<Value> = entries
        .iter()
        .map(|e| {
            let stale = chrono::DateTime::parse_from_rfc3339(&e.last_activity_at)
                .map(|dt| {
                    let age =
                        chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
                    age.num_hours() >= 4
                })
                .unwrap_or(false);
            json!({
                "id": e.id,
                "name": e.name,
                "project_id": e.project_id,
                "owner": e.owner,
                "started_at": e.started_at,
                "last_activity_at": e.last_activity_at,
                "stale": stale
            })
        })
        .collect();

    Ok(json!({
        "active": true,
        "count": summaries.len(),
        "breadcrumbs": summaries
    }))
}

// ── backup ─────────────────────────────────────────────────────────────────────

/// Snapshot the current state of a breadcrumb to `C:\CPC\backups\breadcrumbs\`.
pub fn backup(breadcrumb_id: Option<&str>) -> Result<Value, BreadcrumbError> {
    let (bc_id, pid) = resolve(breadcrumb_id)?;
    let bcs = load_project(&pid);
    let bc = bcs
        .iter()
        .find(|b| b.id == bc_id)
        .ok_or_else(|| BreadcrumbError::NotFound { id: bc_id.clone() })?;

    let backup_dir = std::path::PathBuf::from(r"C:\CPC\backups\breadcrumbs");
    std::fs::create_dir_all(&backup_dir).map_err(BreadcrumbError::Io)?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = backup_dir.join(format!("{}_{}.json", bc_id, ts));
    let content = serde_json::to_string_pretty(bc).map_err(BreadcrumbError::Serde)?;
    std::fs::write(&path, content).map_err(BreadcrumbError::Io)?;

    Ok(json!({
        "status": "backed_up",
        "breadcrumb_id": bc_id,
        "path": path.to_string_lossy()
    }))
}

// ── adopt ──────────────────────────────────────────────────────────────────────

/// Reassign ownership of a breadcrumb to the current actor.
/// Useful when picking up an operation abandoned by another session.
///
/// If the breadcrumb is already archived, returns a descriptive error
/// explaining when and how it was archived.
pub fn adopt(breadcrumb_id: &str, ctx: &WriterContext) -> Result<Value, BreadcrumbError> {
    let resolve_result = resolve(Some(breadcrumb_id));

    // D3: If not found in active index, check archive for descriptive error
    if let Err(BreadcrumbError::NotFound { ref id }) = resolve_result {
        if let Some((bc, path)) = archive::find_archived(id, 7) {
            let result_type = if bc.aborted { "aborted" } else { "completed" };
            return Err(BreadcrumbError::Other(format!(
                "Cannot adopt: breadcrumb {} was already {} on {}. Archived at {}.",
                id,
                result_type,
                bc.last_activity_at,
                path.display()
            )));
        }
    }

    let (bc_id, pid) = resolve_result?;
    let now = now_rfc3339();
    let mut prev_owner = String::new();

    locked_write_project(&pid, |bcs| {
        let bc = bcs
            .iter_mut()
            .find(|b| b.id == bc_id)
            .ok_or_else(|| BreadcrumbError::NotFound { id: bc_id.clone() })?;

        prev_owner = bc.owner.clone();
        bc.owner = ctx.actor.clone();
        bc.writer_actor = ctx.actor.clone();
        bc.writer_machine = ctx.machine.clone();
        bc.writer_session = ctx.session.clone();
        bc.writer_at = now.clone();
        bc.last_activity_at = now.clone();
        Ok(())
    })?;

    // Update index
    let mut index = read_index();
    if let Some(entry) = index.get_mut(&bc_id) {
        entry.owner = ctx.actor.clone();
        entry.last_activity_at = now;
    }
    write_index_silent(&index);

    Ok(json!({
        "status": "adopted",
        "breadcrumb_id": bc_id,
        "new_owner": ctx.actor,
        "prev_owner": prev_owner
    }))
}

// ── list ───────────────────────────────────────────────────────────────────────

/// List breadcrumbs. scope: "active" (default) | "today" | "week" | "all"
///
/// - "active": reads ONLY `active.index.json` (no project JSONL, no archive)
/// - "today"/"week": union of active index + completed archive for the window
/// - "all": active + all completed archive (project JSONL is audit-only, not read)
pub fn list(scope: Option<&str>) -> Result<Value, BreadcrumbError> {
    let scope = scope.unwrap_or("active");

    // Active entries from index
    let mut results: Vec<Value> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // All scopes include active entries
    let index = read_index();
    for entry in index.values() {
        seen_ids.insert(entry.id.clone());
        results.push(json!({
            "id": entry.id,
            "name": entry.name,
            "project_id": entry.project_id,
            "owner": entry.owner,
            "started_at": entry.started_at,
            "last_activity_at": entry.last_activity_at,
            "active": true,
            "aborted": false
        }));
    }

    // For active-only scope, return now
    if scope == "active" {
        results.sort_by(|a, b| {
            let ta = a["started_at"].as_str().unwrap_or("");
            let tb = b["started_at"].as_str().unwrap_or("");
            tb.cmp(ta)
        });
        return Ok(json!({
            "scope": scope,
            "count": results.len(),
            "breadcrumbs": results
        }));
    }

    // For today/week/all — also read completed archive
    let base = archive::base();
    if base.exists() {
        let today = chrono::Local::now().date_naive();
        let cutoff = match scope {
            "today" => Some(today),
            "week" => Some(today - chrono::Duration::days(7)),
            _ => None, // "all"
        };

        if let Ok(date_dirs) = std::fs::read_dir(&base) {
            for date_dir in date_dirs.flatten() {
                if !date_dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                if let Some(cutoff_date) = cutoff {
                    let dir_name = date_dir.file_name().to_string_lossy().to_string();
                    if let Ok(dir_date) = chrono::NaiveDate::parse_from_str(&dir_name, "%Y-%m-%d") {
                        if dir_date < cutoff_date {
                            continue;
                        }
                    }
                }
                if let Ok(files) = std::fs::read_dir(date_dir.path()) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("json") {
                            continue;
                        }
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(bc) = serde_json::from_str::<Breadcrumb>(&content) {
                                if seen_ids.insert(bc.id.clone()) {
                                    results.push(json!({
                                        "id": bc.id,
                                        "name": bc.name,
                                        "project_id": bc.project_id,
                                        "owner": bc.owner,
                                        "started_at": bc.started_at,
                                        "last_activity_at": bc.last_activity_at,
                                        "steps_completed": bc.step_results.len(),
                                        "total_steps": bc.total_steps,
                                        "aborted": bc.aborted,
                                        "active": false
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    results.sort_by(|a, b| {
        let ta = a["started_at"].as_str().unwrap_or("");
        let tb = b["started_at"].as_str().unwrap_or("");
        tb.cmp(ta)
    });

    Ok(json!({
        "scope": scope,
        "count": results.len(),
        "breadcrumbs": results
    }))
}

// ── public helpers (for server wrappers) ──────────────────────────────────────

/// Read the active index (bc_id → IndexEntry). Exposed for server wrappers that
/// need to inspect active breadcrumbs without going through higher-level API.
pub fn read_active_index() -> std::collections::HashMap<String, IndexEntry> {
    storage::read_index()
}

/// Load all breadcrumbs from a project file. Exposed for server wrappers.
pub fn load_project_bcs(project_id: &str) -> Vec<Breadcrumb> {
    storage::load_project(project_id)
}

// ── internal helpers ───────────────────────────────────────────────────────────

fn write_index_silent(index: &std::collections::HashMap<String, IndexEntry>) {
    let _ = storage::write_index(index);
}

// ── D4: Reconcile ─────────────────────────────────────────────────────────────

/// A stale breadcrumb entry found during reconciliation.
#[derive(Debug, Clone, Serialize)]
pub struct StaleEntry {
    pub id: String,
    pub name: String,
    pub last_activity_at: String,
    pub hours_stale: u64,
    pub project_id: String,
}

/// Report returned by `reconcile()`.
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub scanned: usize,
    pub stale_found: Vec<StaleEntry>,
    pub handoff_path: Option<std::path::PathBuf>,
    pub handoff_entries_written: usize,
}

/// Reconcile stale breadcrumbs from `active.index.json`.
///
/// Entries with `last_activity_at` (or `started_at` fallback) older than
/// `stale_threshold_hours` AND not updated within the last 30 minutes
/// are removed from the active index and written to a handoff file at
/// `Volumes/handoffs/pending_breadcrumbs_YYYY-MM-DD.md`.
pub fn reconcile(stale_threshold_hours: u64) -> ReconcileReport {
    let mut index = read_index();
    let now = chrono::Utc::now();
    let threshold = chrono::Duration::hours(stale_threshold_hours as i64);
    let thirty_min = chrono::Duration::minutes(30);

    let scanned = index.len();
    let mut stale_entries: Vec<StaleEntry> = Vec::new();

    // Identify stale entries
    let stale_ids: Vec<String> = index
        .iter()
        .filter_map(|(id, entry)| {
            let last = chrono::DateTime::parse_from_rfc3339(&entry.last_activity_at)
                .or_else(|_| chrono::DateTime::parse_from_rfc3339(&entry.started_at))
                .ok()?;
            let age = now.signed_duration_since(last.with_timezone(&chrono::Utc));

            // Must be older than threshold AND not updated within last 30 min
            if age > threshold && age > thirty_min {
                let hours_stale = age.num_hours().max(0) as u64;
                stale_entries.push(StaleEntry {
                    id: id.clone(),
                    name: entry.name.clone(),
                    last_activity_at: entry.last_activity_at.clone(),
                    hours_stale,
                    project_id: entry
                        .project_id
                        .as_deref()
                        .unwrap_or("ungrouped")
                        .to_string(),
                });
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();

    if stale_entries.is_empty() {
        return ReconcileReport {
            scanned,
            stale_found: Vec::new(),
            handoff_path: None,
            handoff_entries_written: 0,
        };
    }

    // Write handoff file
    let handoff_path = write_handoff_file(&stale_entries);
    let entries_written = stale_entries.len();

    // Remove stale entries from index
    for id in &stale_ids {
        index.remove(id);
    }
    let _ = storage::write_index(&index);

    ReconcileReport {
        scanned,
        stale_found: stale_entries,
        handoff_path,
        handoff_entries_written: entries_written,
    }
}

/// Write stale entries to `Volumes/handoffs/pending_breadcrumbs_YYYY-MM-DD.md`.
fn write_handoff_file(entries: &[StaleEntry]) -> Option<std::path::PathBuf> {
    let handoffs_dir = std::env::var("CPC_BREADCRUMB_HANDOFF_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            cpc_paths::volumes_path()
                .map(|v| v.join("handoffs"))
                .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\My Drive\Volumes\handoffs"))
        });

    if std::fs::create_dir_all(&handoffs_dir).is_err() {
        return None;
    }

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = handoffs_dir.join(format!("pending_breadcrumbs_{}.md", date));

    let mut content = String::new();

    // If file doesn't exist, write header
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.is_empty() {
        content.push_str(&format!("# Pending Breadcrumbs - {}\n\n", date));
    } else {
        content = existing;
        if !content.ends_with('\n') {
            content.push('\n');
        }
    }

    for entry in entries {
        content.push_str(&format!("## {}\n", entry.name));
        content.push_str(&format!("- ID: {}\n", entry.id));
        content.push_str(&format!("- Last activity: {}\n", entry.last_activity_at));
        content.push_str(&format!("- Hours stale: {}\n", entry.hours_stale));
        content.push_str(&format!("- Project: {}\n", entry.project_id));
        content.push_str("\n---\n\n");
    }

    std::fs::write(&path, content).ok()?;
    Some(path)
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// Override state dir to a temp location for tests.
    /// We rely on the fact that tests in Rust run in the package's target dir.
    fn ensure_test_state() {
        // Tests use the real state dir — acceptable for integration tests.
        let _ = ensure_dirs();
    }

    #[test]
    fn test_slugify() {
        assert_eq!(schema::slugify("Hello World!", 40), "hello_world");
        // hyphens are allowed chars — preserved as-is
        assert_eq!(schema::slugify("foo--bar", 40), "foo--bar");
        assert_eq!(schema::slugify("", 40), "operation");
        let long = "a".repeat(50);
        assert_eq!(schema::slugify(&long, 40).len(), 40);
    }

    #[test]
    fn test_new_id_format() {
        let id = schema::new_id("My Operation");
        assert!(id.starts_with("bc_"), "id should start with bc_: {}", id);
        let parts: Vec<&str> = id.splitn(3, '_').collect();
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn test_concurrent_different_projects() {
        // Two threads writing to different projects should not block each other.
        ensure_test_state();
        let barrier = Arc::new(Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();

        let t1 = std::thread::spawn(move || {
            b1.wait();
            locked_write_project("test_proj_a", |bcs| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                bcs.push(make_test_bc("bc_test_a", "test_proj_a"));
                Ok(())
            })
        });
        let t2 = std::thread::spawn(move || {
            b2.wait();
            locked_write_project("test_proj_b", |bcs| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                bcs.push(make_test_bc("bc_test_b", "test_proj_b"));
                Ok(())
            })
        });

        let r1 = t1.join().expect("t1 panicked");
        let r2 = t2.join().expect("t2 panicked");
        assert!(r1.is_ok(), "Project A write failed: {:?}", r1);
        assert!(r2.is_ok(), "Project B write failed: {:?}", r2);

        // Cleanup
        let _ = std::fs::remove_file(storage::project_file("test_proj_a"));
        let _ = std::fs::remove_file(storage::project_file("test_proj_b"));
    }

    #[test]
    fn test_concurrent_same_project_serializes() {
        // Two threads writing to same project: should serialize (not corrupt).
        ensure_test_state();
        let barrier = Arc::new(Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();

        let t1 = std::thread::spawn(move || {
            b1.wait();
            locked_write_project("test_proj_serial", |bcs| {
                std::thread::sleep(std::time::Duration::from_millis(30));
                bcs.push(make_test_bc("bc_serial_1", "test_proj_serial"));
                Ok(())
            })
        });
        let t2 = std::thread::spawn(move || {
            b2.wait();
            locked_write_project("test_proj_serial", |bcs| {
                std::thread::sleep(std::time::Duration::from_millis(30));
                bcs.push(make_test_bc("bc_serial_2", "test_proj_serial"));
                Ok(())
            })
        });

        let r1 = t1.join().expect("t1 panicked");
        let r2 = t2.join().expect("t2 panicked");

        // At least one should succeed; both may succeed (sequential)
        let succeeded = r1.is_ok() as usize + r2.is_ok() as usize;
        assert!(succeeded >= 1, "At least one write should succeed");

        // Read back and verify no corruption
        let bcs = load_project("test_proj_serial");
        // Should not have corrupted entries
        for bc in &bcs {
            assert!(!bc.id.is_empty(), "Got empty id in project file");
        }

        let _ = std::fs::remove_file(storage::project_file("test_proj_serial"));
    }

    #[test]
    fn test_conflict_detection() {
        let bc = make_test_bc_with_session("bc_conf_test", "_ungrouped", "session_other");
        // Same session → no conflict
        assert!(conflict::check(&bc, "session_other").is_none());
        // Different session, last_activity_at just now → conflict
        let info = conflict::check(&bc, "session_mine");
        assert!(
            info.is_some(),
            "Expected conflict for different session within 30s"
        );
    }

    #[test]
    fn test_stale_detection() {
        let mut bc = make_test_bc("bc_stale_test", "_ungrouped");
        // Set last_activity_at to 5 hours ago
        let five_hours_ago = chrono::Utc::now() - chrono::Duration::hours(5);
        bc.last_activity_at = five_hours_ago.to_rfc3339();
        assert!(bc.is_stale(), "5h old breadcrumb should be stale");

        let mut bc2 = make_test_bc("bc_fresh_test", "_ungrouped");
        bc2.last_activity_at = chrono::Utc::now().to_rfc3339();
        assert!(
            !bc2.is_stale(),
            "Just-created breadcrumb should not be stale"
        );
    }

    // ── helpers ────────────────────────────────────────────────────────────────

    fn make_test_bc(id: &str, project_id: &str) -> Breadcrumb {
        make_test_bc_with_session(id, project_id, "session_default")
    }

    fn make_test_bc_with_session(id: &str, project_id: &str, session: &str) -> Breadcrumb {
        let now = chrono::Utc::now().to_rfc3339();
        Breadcrumb {
            id: id.to_string(),
            name: format!("Test BC {}", id),
            project_id: if project_id == "_ungrouped" {
                None
            } else {
                Some(project_id.to_string())
            },
            owner: "test_actor".to_string(),
            writer_actor: "test_actor".to_string(),
            writer_machine: "test_machine".to_string(),
            writer_session: session.to_string(),
            writer_at: now.clone(),
            started_at: now.clone(),
            last_activity_at: now,
            steps: vec!["step1".to_string(), "step2".to_string()],
            current_step: 0,
            total_steps: 2,
            step_results: Vec::new(),
            files_changed: Vec::new(),
            stale: false,
            conflict_warning: None,
            aborted: false,
            abort_reason: None,
            auto_started: false,
        }
    }

    // ── D1-D4 isolated tests ──────────────────────────────────────────────────
    //
    // These use env-var overrides + tempdir so they never touch live state.
    // A global mutex serializes them since env vars are process-wide.

    use std::sync::Mutex;
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Set up isolated env pointing at tempdir, run closure, then restore.
    fn with_isolated_env<F: FnOnce(&std::path::Path, &std::path::Path, &std::path::Path)>(f: F) {
        let _guard = TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        let archive_dir = tmp.path().join("archive");
        let handoff_dir = tmp.path().join("handoffs");
        std::fs::create_dir_all(&state_dir.join("projects")).unwrap();
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::create_dir_all(&handoff_dir).unwrap();

        std::env::set_var("CPC_BREADCRUMB_STATE_DIR", &state_dir);
        std::env::set_var("CPC_BREADCRUMB_ARCHIVE_DIR", &archive_dir);
        std::env::set_var("CPC_BREADCRUMB_HANDOFF_DIR", &handoff_dir);

        f(&state_dir, &archive_dir, &handoff_dir);

        std::env::remove_var("CPC_BREADCRUMB_STATE_DIR");
        std::env::remove_var("CPC_BREADCRUMB_ARCHIVE_DIR");
        std::env::remove_var("CPC_BREADCRUMB_HANDOFF_DIR");
    }

    #[test]
    fn test_d1_abort_archives_with_aborted_flag() {
        with_isolated_env(|_state, archive, _handoff| {
            let ctx = WriterContext::new("tester", "test_machine", "sess_d1");

            // Start a breadcrumb
            let result = start("D1 test op", vec!["s1".into(), "s2".into()], None, &ctx).unwrap();
            let id = result["id"].as_str().unwrap().to_string();

            // Abort it
            let abort_result = abort("testing abort", Some(&id), &ctx).unwrap();
            assert_eq!(abort_result["status"], "aborted");

            // Verify the archived file has aborted=true
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let archived = archive.join(&today).join(format!("{}.json", id));
            assert!(archived.exists(), "Archive file should exist");
            let content = std::fs::read_to_string(&archived).unwrap();
            let bc: Breadcrumb = serde_json::from_str(&content).unwrap();
            assert!(bc.aborted, "Archived breadcrumb should have aborted=true");
            assert_eq!(bc.abort_reason.as_deref(), Some("testing abort"));

            // Now start and complete one — should NOT have aborted flag
            let result2 = start("D1 complete op", vec!["s1".into()], None, &ctx).unwrap();
            let id2 = result2["id"].as_str().unwrap().to_string();
            let _ = complete("done", Some(&id2), &ctx).unwrap();

            let archived2 = archive.join(&today).join(format!("{}.json", id2));
            assert!(archived2.exists(), "Completed archive file should exist");
            let content2 = std::fs::read_to_string(&archived2).unwrap();
            let bc2: Breadcrumb = serde_json::from_str(&content2).unwrap();
            assert!(
                !bc2.aborted,
                "Completed breadcrumb should have aborted=false"
            );
        });
    }

    #[test]
    fn test_d2_list_active_scope_reads_only_index() {
        with_isolated_env(|_state, archive, _handoff| {
            let ctx = WriterContext::new("tester", "test_machine", "sess_d2");

            // Start two breadcrumbs (active)
            let r1 = start("Active BC 1", vec!["s1".into()], None, &ctx).unwrap();
            let r2 = start("Active BC 2", vec!["s1".into()], None, &ctx).unwrap();

            // Manually place a fake completed archive entry
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let day_dir = archive.join(&today);
            std::fs::create_dir_all(&day_dir).unwrap();
            let mut fake_bc = make_test_bc("bc_archived_fake", "_ungrouped");
            fake_bc.name = "Archived Fake".to_string();
            std::fs::write(
                day_dir.join("bc_archived_fake.json"),
                serde_json::to_string_pretty(&fake_bc).unwrap(),
            )
            .unwrap();

            // list(scope="active") should return ONLY the 2 active entries
            let active_list = list(Some("active")).unwrap();
            assert_eq!(
                active_list["count"], 2,
                "active scope should have 2 entries"
            );
            let bcs = active_list["breadcrumbs"].as_array().unwrap();
            let ids: Vec<&str> = bcs.iter().map(|b| b["id"].as_str().unwrap()).collect();
            assert!(
                !ids.contains(&"bc_archived_fake"),
                "archived entry should NOT appear in active scope"
            );

            // list(scope="today") should return all 3 (2 active + 1 archived)
            let today_list = list(Some("today")).unwrap();
            assert_eq!(today_list["count"], 3, "today scope should have 3 entries");

            // status() should also return only index entries
            let st = status(None).unwrap();
            assert_eq!(st["count"], 2, "status should show 2 active entries");

            let _ = r1;
            let _ = r2;
        });
    }

    #[test]
    fn test_d3_abort_archived_returns_already_archived() {
        with_isolated_env(|_state, archive, _handoff| {
            let ctx = WriterContext::new("tester", "test_machine", "sess_d3");

            // Create a fake archived breadcrumb (completed)
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let day_dir = archive.join(&today);
            std::fs::create_dir_all(&day_dir).unwrap();
            let mut completed_bc = make_test_bc("bc_completed_old", "_ungrouped");
            completed_bc.name = "Old Completed".to_string();
            std::fs::write(
                day_dir.join("bc_completed_old.json"),
                serde_json::to_string_pretty(&completed_bc).unwrap(),
            )
            .unwrap();

            // Create a fake archived breadcrumb (aborted)
            let mut aborted_bc = make_test_bc("bc_aborted_old", "_ungrouped");
            aborted_bc.name = "Old Aborted".to_string();
            aborted_bc.aborted = true;
            aborted_bc.abort_reason = Some("stale".to_string());
            std::fs::write(
                day_dir.join("bc_aborted_old.json"),
                serde_json::to_string_pretty(&aborted_bc).unwrap(),
            )
            .unwrap();

            // abort on completed archived ID → already_archived
            let result = abort("trying again", Some("bc_completed_old"), &ctx).unwrap();
            assert_eq!(result["status"], "already_archived");
            assert_eq!(result["result_type"], "completed");

            // abort on aborted archived ID → already_archived with aborted type
            let result2 = abort("trying again", Some("bc_aborted_old"), &ctx).unwrap();
            assert_eq!(result2["status"], "already_archived");
            assert_eq!(result2["result_type"], "aborted");

            // adopt on archived ID → descriptive error
            let adopt_err = adopt("bc_completed_old", &ctx);
            assert!(adopt_err.is_err(), "adopt on archived should fail");
            let err_msg = adopt_err.unwrap_err().to_string();
            assert!(
                err_msg.contains("Cannot adopt"),
                "Error should contain 'Cannot adopt': {}",
                err_msg
            );
            assert!(
                err_msg.contains("completed"),
                "Error should mention result type: {}",
                err_msg
            );

            // abort on truly non-existent ID → normal NotFound error
            let not_found = abort("nope", Some("bc_does_not_exist"), &ctx);
            assert!(not_found.is_err(), "Non-existent ID should error");
        });
    }

    #[test]
    fn test_d4_reconcile_moves_stale_to_handoff() {
        with_isolated_env(|_state, _archive, _handoff| {
            // Manually populate the active index with 3 entries:
            // 1. fresh (just now)
            // 2. stale + untouched (50h ago)
            // 3. stale but recently updated (50h started, but last_activity 10 min ago)
            let now = chrono::Utc::now();
            let fresh_time = now.to_rfc3339();
            let stale_time = (now - chrono::Duration::hours(50)).to_rfc3339();
            let recent_time = (now - chrono::Duration::minutes(10)).to_rfc3339();

            let mut index = std::collections::HashMap::new();
            index.insert(
                "bc_fresh".to_string(),
                IndexEntry {
                    id: "bc_fresh".to_string(),
                    project_id: None,
                    name: "Fresh BC".to_string(),
                    owner: "tester".to_string(),
                    last_activity_at: fresh_time.clone(),
                    started_at: fresh_time,
                },
            );
            index.insert(
                "bc_stale_untouched".to_string(),
                IndexEntry {
                    id: "bc_stale_untouched".to_string(),
                    project_id: Some("proj_a".to_string()),
                    name: "Stale Untouched BC".to_string(),
                    owner: "tester".to_string(),
                    last_activity_at: stale_time.clone(),
                    started_at: stale_time,
                },
            );
            index.insert(
                "bc_stale_recent".to_string(),
                IndexEntry {
                    id: "bc_stale_recent".to_string(),
                    project_id: None,
                    name: "Stale But Recent BC".to_string(),
                    owner: "tester".to_string(),
                    last_activity_at: recent_time,
                    started_at: (now - chrono::Duration::hours(50)).to_rfc3339(),
                },
            );

            storage::write_index(&index).unwrap();

            // Run reconcile with 48h threshold
            let report = reconcile(48);

            assert_eq!(report.scanned, 3, "Should scan 3 entries");
            assert_eq!(
                report.stale_found.len(),
                1,
                "Only 1 entry should be stale (bc_stale_untouched)"
            );
            assert_eq!(report.stale_found[0].id, "bc_stale_untouched");
            assert!(
                report.stale_found[0].hours_stale >= 49,
                "Should be ~50h stale"
            );
            assert_eq!(report.handoff_entries_written, 1);
            assert!(
                report.handoff_path.is_some(),
                "Handoff file should be written"
            );

            // Verify active index lost the stale entry
            let updated_index = storage::read_index();
            assert_eq!(
                updated_index.len(),
                2,
                "Index should have 2 entries after reconcile"
            );
            assert!(
                updated_index.contains_key("bc_fresh"),
                "Fresh entry should remain"
            );
            assert!(
                updated_index.contains_key("bc_stale_recent"),
                "Recent-activity entry should remain"
            );
            assert!(
                !updated_index.contains_key("bc_stale_untouched"),
                "Stale entry should be removed"
            );

            // Verify handoff file content
            let handoff_path = report.handoff_path.unwrap();
            assert!(handoff_path.exists(), "Handoff file should exist");
            let content = std::fs::read_to_string(&handoff_path).unwrap();
            assert!(
                content.contains("# Pending Breadcrumbs"),
                "Should have markdown header"
            );
            assert!(
                content.contains("Stale Untouched BC"),
                "Should contain the stale breadcrumb name"
            );
            assert!(
                content.contains("bc_stale_untouched"),
                "Should contain the stale breadcrumb ID"
            );
            assert!(content.contains("proj_a"), "Should contain the project ID");
        });
    }
}
