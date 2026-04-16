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

pub mod error;
pub mod schema;
mod archive;
mod conflict;
mod storage;

pub use error::BreadcrumbError;
pub use schema::{Breadcrumb, ConflictInfo, IndexEntry};

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
    pub fn new(actor: impl Into<String>, machine: impl Into<String>, session: impl Into<String>) -> Self {
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
    let pid = project_id.clone().unwrap_or_else(|| "_ungrouped".to_string());
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
    let pid = project_id.clone().unwrap_or_else(|| "_ungrouped".to_string());
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
pub fn abort(
    reason: &str,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let (bc_id, pid) = resolve(breadcrumb_id)?;
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

/// Get status of active breadcrumbs.
///
/// `project_id`: filter to a specific project.
/// `scope`: "active" (default) | "today" | "week" | "all"
pub fn status(project_id: Option<&str>, scope: Option<&str>) -> Result<Value, BreadcrumbError> {
    let scope = scope.unwrap_or("active");

    if scope == "active" {
        let mut all = if let Some(pid) = project_id {
            load_project(pid)
        } else {
            load_all_active()
        };
        // Compute stale flag
        for bc in &mut all {
            bc.stale = bc.is_stale();
        }

        if all.is_empty() {
            return Ok(json!({ "active": false, "breadcrumbs": [] }));
        }

        let summaries: Vec<Value> = all
            .iter()
            .map(|bc| {
                json!({
                    "id": bc.id,
                    "name": bc.name,
                    "project_id": bc.project_id,
                    "owner": bc.owner,
                    "current_step": bc.current_step,
                    "total_steps": bc.total_steps,
                    "next_step": bc.steps.get(bc.current_step),
                    "started_at": bc.started_at,
                    "last_activity_at": bc.last_activity_at,
                    "stale": bc.stale,
                    "files_changed": bc.files_changed
                })
            })
            .collect();

        return Ok(json!({
            "active": true,
            "count": summaries.len(),
            "breadcrumbs": summaries
        }));
    }

    // For "today", "week", "all" — read from archive
    list(Some(scope))
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
pub fn adopt(breadcrumb_id: &str, ctx: &WriterContext) -> Result<Value, BreadcrumbError> {
    let (bc_id, pid) = resolve(Some(breadcrumb_id))?;
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

/// List breadcrumbs from archive. scope: "today" | "week" | "all"
pub fn list(scope: Option<&str>) -> Result<Value, BreadcrumbError> {
    let scope = scope.unwrap_or("today");
    let base = archive::base();
    if !base.exists() {
        return Ok(json!({ "scope": scope, "count": 0, "breadcrumbs": [] }));
    }

    let today = chrono::Local::now().date_naive();
    let cutoff = match scope {
        "today" => Some(today),
        "week" => Some(today - chrono::Duration::days(7)),
        _ => None, // "all"
    };

    let mut results: Vec<Value> = Vec::new();

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
                            results.push(json!({
                                "id": bc.id,
                                "name": bc.name,
                                "project_id": bc.project_id,
                                "owner": bc.owner,
                                "started_at": bc.started_at,
                                "last_activity_at": bc.last_activity_at,
                                "steps_completed": bc.step_results.len(),
                                "total_steps": bc.total_steps,
                                "aborted": bc.aborted
                            }));
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
        assert!(info.is_some(), "Expected conflict for different session within 30s");
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
        assert!(!bc2.is_stale(), "Just-created breadcrumb should not be stale");
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
            project_id: if project_id == "_ungrouped" { None } else { Some(project_id.to_string()) },
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
}
