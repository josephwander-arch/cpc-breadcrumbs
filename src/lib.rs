//! cpc-breadcrumbs — shared breadcrumb tracking for CPC MCP servers.
//!
//! v0.3.0: Unified one-file-per-breadcrumb storage on Google Drive for Desktop.
//!
//! # Storage layout
//! Active:  `C:\My Drive\Volumes\breadcrumbs\active\bc_{id}.json`
//! Archive: `C:\My Drive\Volumes\breadcrumbs\completed\{YYYY-MM-DD}\bc_{id}.json`
//!
//! Writes use atomic tmp+rename. No file locking. Last-writer-wins is acceptable
//! for idempotent breadcrumb updates.
//!
//! # Backward compatibility
//! On first init(), `migrate_legacy()` reads the old dual-store
//! (active.index.json + projects/*.jsonl) and splits each breadcrumb into
//! an individual file. Orphans (in JSONL but not in index) are migrated too.

pub mod error;
pub mod schema;
mod archive;
mod conflict;
pub mod storage;

pub use error::BreadcrumbError;
pub use schema::{Breadcrumb, ConflictInfo, IndexEntry};

use serde_json::{json, Value};

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

/// Must be called on server startup. Creates storage dirs, runs legacy migration,
/// and optionally reaps stale breadcrumbs.
pub fn init() {
    if let Err(e) = storage::ensure_dirs() {
        eprintln!("[cpc-breadcrumbs] Failed to create storage dirs: {}", e);
    }

    // Run legacy migration (idempotent — no-op if already migrated)
    storage::migrate_legacy();

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
    storage::active_count() > 0
}

pub fn active_count() -> usize {
    storage::active_count()
}

/// Return a snapshot of all currently active breadcrumbs.
pub fn list_active() -> Vec<Breadcrumb> {
    storage::load_all_active()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ── start ──────────────────────────────────────────────────────────────────────

pub fn start(
    name: &str,
    steps: Vec<String>,
    project_id: Option<String>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    storage::ensure_dirs()?;

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
        last_activity_at: now,
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

    storage::write_breadcrumb(&bc)?;

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

pub fn step(
    result: &str,
    files_changed: Vec<String>,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let bc_id = storage::resolve(breadcrumb_id)?;
    let now = now_rfc3339();
    let mut out_step_name = String::new();
    let mut out_current = 0usize;
    let mut out_total = 0usize;
    let mut conflict_info: Option<ConflictInfo> = None;

    let bc = storage::mutate_breadcrumb(&bc_id, |bc| {
        // Conflict detection
        conflict_info = conflict::check(bc, &ctx.session);

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
        if let Some(ref c) = conflict_info {
            bc.conflict_warning = Some(c.clone());
        }

        out_step_name = step_name;
        out_current = bc.current_step;
        out_total = bc.total_steps;

        Ok(())
    })?;
    let _ = bc; // used for the write

    let remaining = out_total.saturating_sub(out_current);

    let mut resp = json!({
        "step_completed": out_step_name,
        "current": out_current,
        "total": out_total,
        "remaining": remaining,
        "breadcrumb_id": bc_id
    });

    if let Some(c) = conflict_info {
        resp["conflict_warning"] = serde_json::to_value(&c).unwrap_or(Value::Null);
    }

    Ok(resp)
}

// ── complete ───────────────────────────────────────────────────────────────────

pub fn complete(
    summary: &str,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let bc_id = storage::resolve(breadcrumb_id)?;
    let now = now_rfc3339();

    let mut bc = storage::read_breadcrumb(&bc_id)?;
    bc.last_activity_at = now.clone();
    bc.writer_actor = ctx.actor.clone();
    bc.writer_machine = ctx.machine.clone();
    bc.writer_session = ctx.session.clone();
    bc.writer_at = now;
    bc.stale = false;

    let bc_name = bc.name.clone();
    let files_changed = bc.files_changed.clone();
    let steps_completed = bc.step_results.len();

    let archived_path = archive::archive(&bc)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

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

/// Like `start` but with auto_started flag.
pub fn start_auto(
    name: &str,
    steps: Vec<String>,
    project_id: Option<String>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    storage::ensure_dirs()?;

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
        last_activity_at: now,
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

    storage::write_breadcrumb(&bc)?;

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

pub fn abort(
    reason: &str,
    breadcrumb_id: Option<&str>,
    ctx: &WriterContext,
) -> Result<Value, BreadcrumbError> {
    let bc_id = storage::resolve(breadcrumb_id)?;
    let now = now_rfc3339();

    let mut bc = storage::read_breadcrumb(&bc_id)?;
    bc.aborted = true;
    bc.abort_reason = Some(reason.to_string());
    bc.last_activity_at = now.clone();
    bc.writer_actor = ctx.actor.clone();
    bc.writer_session = ctx.session.clone();
    bc.writer_at = now;

    let bc_name = bc.name.clone();
    let steps_completed = bc.step_results.len();

    let _ = archive::archive(&bc);

    Ok(json!({
        "status": "aborted",
        "id": bc_id,
        "name": bc_name,
        "reason": reason,
        "steps_completed": steps_completed
    }))
}

// ── status ─────────────────────────────────────────────────────────────────────

pub fn status(project_id: Option<&str>, scope: Option<&str>) -> Result<Value, BreadcrumbError> {
    let scope = scope.unwrap_or("active");

    if scope == "active" {
        let mut all = storage::load_all_active();

        // Filter by project_id if specified
        if let Some(pid) = project_id {
            all.retain(|bc| {
                bc.project_id.as_deref().unwrap_or("_ungrouped") == pid
            });
        }

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

pub fn backup(breadcrumb_id: Option<&str>) -> Result<Value, BreadcrumbError> {
    let bc_id = storage::resolve(breadcrumb_id)?;
    let bc = storage::read_breadcrumb(&bc_id)?;

    let backup_dir = std::path::PathBuf::from(r"C:\CPC\backups\breadcrumbs");
    std::fs::create_dir_all(&backup_dir).map_err(BreadcrumbError::Io)?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = backup_dir.join(format!("{}_{}.json", bc_id, ts));
    let content = serde_json::to_string_pretty(&bc).map_err(BreadcrumbError::Serde)?;
    std::fs::write(&path, content).map_err(BreadcrumbError::Io)?;

    Ok(json!({
        "status": "backed_up",
        "breadcrumb_id": bc_id,
        "path": path.to_string_lossy()
    }))
}

// ── adopt ──────────────────────────────────────────────────────────────────────

pub fn adopt(breadcrumb_id: &str, ctx: &WriterContext) -> Result<Value, BreadcrumbError> {
    let bc_id = storage::resolve(Some(breadcrumb_id))?;
    let now = now_rfc3339();
    let mut prev_owner = String::new();

    storage::mutate_breadcrumb(&bc_id, |bc| {
        prev_owner = bc.owner.clone();
        bc.owner = ctx.actor.clone();
        bc.writer_actor = ctx.actor.clone();
        bc.writer_machine = ctx.machine.clone();
        bc.writer_session = ctx.session.clone();
        bc.writer_at = now.clone();
        bc.last_activity_at = now.clone();
        Ok(())
    })?;

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
        _ => None,
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

/// Read all active breadcrumbs as a map (bc_id → IndexEntry).
/// Backward-compatible API for consumers that used read_active_index().
pub fn read_active_index() -> std::collections::HashMap<String, IndexEntry> {
    let all = storage::load_all_active();
    let mut map = std::collections::HashMap::new();
    for bc in all {
        map.insert(bc.id.clone(), IndexEntry {
            id: bc.id,
            project_id: bc.project_id,
            name: bc.name,
            owner: bc.owner,
            last_activity_at: bc.last_activity_at,
            started_at: bc.started_at,
        });
    }
    map
}

/// Load all breadcrumbs from a project. Backward-compatible API.
pub fn load_project_bcs(project_id: &str) -> Vec<Breadcrumb> {
    storage::load_all_active()
        .into_iter()
        .filter(|bc| bc.project_id.as_deref().unwrap_or("_ungrouped") == project_id)
        .collect()
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn test_ctx() -> WriterContext {
        WriterContext::new("test_actor", "test_machine", "test_session")
    }

    fn setup_sandbox() -> (tempfile::TempDir, tempfile::TempDir) {
        let active_tmp = tempfile::tempdir().unwrap();
        let completed_tmp = tempfile::tempdir().unwrap();
        storage::test_helpers::set_active_dir(active_tmp.path().to_path_buf());
        storage::test_helpers::set_completed_dir(completed_tmp.path().to_path_buf());
        (active_tmp, completed_tmp)
    }

    fn teardown_sandbox() {
        storage::test_helpers::clear_overrides();
    }

    #[test]
    fn test_slugify() {
        assert_eq!(schema::slugify("Hello World!", 40), "hello_world");
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
    fn test_start_and_status() {
        let (_a, _c) = setup_sandbox();
        let ctx = test_ctx();

        let result = start("test op", vec!["s1".into(), "s2".into()], None, &ctx).unwrap();
        assert_eq!(result["status"], "started");
        assert_eq!(active_count(), 1);

        let status = status(None, Some("active")).unwrap();
        assert_eq!(status["count"], 1);

        teardown_sandbox();
    }

    #[test]
    fn test_step_and_complete() {
        let (_a, _c) = setup_sandbox();
        let ctx = test_ctx();

        let result = start("step test", vec!["s1".into(), "s2".into()], None, &ctx).unwrap();
        let id = result["id"].as_str().unwrap();

        let step_result = step("did s1", vec![], Some(id), &ctx).unwrap();
        assert_eq!(step_result["current"], 1);

        let complete_result = complete("all done", Some(id), &ctx).unwrap();
        assert_eq!(complete_result["status"], "completed");
        assert_eq!(active_count(), 0);

        teardown_sandbox();
    }

    #[test]
    fn test_abort() {
        let (_a, _c) = setup_sandbox();
        let ctx = test_ctx();

        let result = start("abort test", vec!["s1".into()], None, &ctx).unwrap();
        let id = result["id"].as_str().unwrap();

        let abort_result = abort("changed plans", Some(id), &ctx).unwrap();
        assert_eq!(abort_result["status"], "aborted");
        assert_eq!(active_count(), 0);

        teardown_sandbox();
    }

    #[test]
    fn test_concurrent_starts() {
        let (_a, _c) = setup_sandbox();
        let barrier = Arc::new(Barrier::new(10));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    let ctx = WriterContext::new("test", "machine", format!("session_{}", i));
                    start(&format!("concurrent_{}", i), vec!["s1".into()], None, &ctx)
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(successes, 10, "All 10 concurrent starts should succeed");
        assert_eq!(active_count(), 10);

        teardown_sandbox();
    }

    #[test]
    fn test_atomic_write_no_partial() {
        let (_a, _c) = setup_sandbox();
        let ctx = test_ctx();

        // Start a breadcrumb normally
        let result = start("atomic test", vec!["s1".into()], None, &ctx).unwrap();
        let id = result["id"].as_str().unwrap().to_string();

        // Verify .json exists and no .tmp lingers
        let active = storage::test_helpers::get_active_dir_override().unwrap();
        let json_path = active.join(format!("{}.json", id));
        let tmp_path = active.join(format!("{}.json.tmp", id));
        assert!(json_path.exists(), ".json file should exist");
        assert!(!tmp_path.exists(), ".tmp file should not linger");

        teardown_sandbox();
    }

    #[test]
    fn test_conflict_detection() {
        let bc = make_test_bc_with_session("bc_conf_test", "_ungrouped", "session_other");
        assert!(conflict::check(&bc, "session_other").is_none());
        let info = conflict::check(&bc, "session_mine");
        assert!(info.is_some(), "Expected conflict for different session within 30s");
    }

    #[test]
    fn test_stale_detection() {
        let mut bc = make_test_bc("bc_stale_test", "_ungrouped");
        let five_hours_ago = chrono::Utc::now() - chrono::Duration::hours(5);
        bc.last_activity_at = five_hours_ago.to_rfc3339();
        assert!(bc.is_stale(), "5h old breadcrumb should be stale");

        let mut bc2 = make_test_bc("bc_fresh_test", "_ungrouped");
        bc2.last_activity_at = chrono::Utc::now().to_rfc3339();
        assert!(!bc2.is_stale(), "Just-created breadcrumb should not be stale");
    }

    #[test]
    fn test_migration_with_orphans() {
        // Set up sandbox
        let active_tmp = tempfile::tempdir().unwrap();
        let completed_tmp = tempfile::tempdir().unwrap();
        let legacy_tmp = tempfile::tempdir().unwrap();

        storage::test_helpers::set_active_dir(active_tmp.path().to_path_buf());
        storage::test_helpers::set_completed_dir(completed_tmp.path().to_path_buf());
        storage::test_helpers::set_legacy_dir(legacy_tmp.path().to_path_buf());

        // Create legacy structure
        let projects_dir = legacy_tmp.path().join("projects");
        std::fs::create_dir_all(&projects_dir).unwrap();

        // Create index with 2 entries
        let mut index: std::collections::HashMap<String, IndexEntry> = std::collections::HashMap::new();
        index.insert("bc_indexed_1".to_string(), IndexEntry {
            id: "bc_indexed_1".to_string(),
            project_id: None,
            name: "indexed op 1".to_string(),
            owner: "test".to_string(),
            last_activity_at: chrono::Utc::now().to_rfc3339(),
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        index.insert("bc_indexed_2".to_string(), IndexEntry {
            id: "bc_indexed_2".to_string(),
            project_id: None,
            name: "indexed op 2".to_string(),
            owner: "test".to_string(),
            last_activity_at: chrono::Utc::now().to_rfc3339(),
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        let index_json = serde_json::to_string_pretty(&index).unwrap();
        std::fs::write(legacy_tmp.path().join("active.index.json"), &index_json).unwrap();

        // Create JSONL with 4 entries (2 indexed + 2 orphans)
        let bc1 = make_test_bc("bc_indexed_1", "_ungrouped");
        let bc2 = make_test_bc("bc_indexed_2", "_ungrouped");
        let orphan1 = make_test_bc("bc_orphan_1776327146", "_ungrouped");
        let orphan2 = make_test_bc("bc_orphan_1776327837", "_ungrouped");

        let mut jsonl = String::new();
        jsonl.push_str(&serde_json::to_string(&bc1).unwrap());
        jsonl.push('\n');
        jsonl.push_str(&serde_json::to_string(&bc2).unwrap());
        jsonl.push('\n');
        jsonl.push_str(&serde_json::to_string(&orphan1).unwrap());
        jsonl.push('\n');
        jsonl.push_str(&serde_json::to_string(&orphan2).unwrap());
        jsonl.push('\n');
        std::fs::write(projects_dir.join("_ungrouped.jsonl"), &jsonl).unwrap();

        // Run migration
        storage::migrate_legacy();

        // Verify: 4 files in active/
        let count = storage::active_count();
        assert_eq!(count, 4, "All 4 breadcrumbs (including orphans) should be migrated");

        // Verify orphans are now individually readable
        let o1 = storage::read_breadcrumb("bc_orphan_1776327146");
        assert!(o1.is_ok(), "Orphan 1 should be readable after migration");
        let o2 = storage::read_breadcrumb("bc_orphan_1776327837");
        assert!(o2.is_ok(), "Orphan 2 should be readable after migration");

        // Verify legacy dir was renamed
        assert!(!legacy_tmp.path().exists() || {
            // Check if renamed (legacy_tmp might still exist as empty parent)
            let parent = legacy_tmp.path().parent().unwrap();
            std::fs::read_dir(parent)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("migrated"))
        });

        teardown_sandbox();
    }

    #[test]
    fn test_orphan_abort_after_migration() {
        // After migration, orphans should be abortable
        let active_tmp = tempfile::tempdir().unwrap();
        let completed_tmp = tempfile::tempdir().unwrap();

        storage::test_helpers::set_active_dir(active_tmp.path().to_path_buf());
        storage::test_helpers::set_completed_dir(completed_tmp.path().to_path_buf());

        // Directly write a "migrated orphan" to active
        let orphan = make_test_bc("bc_1776327146_manager_dashboard_fix3", "_ungrouped");
        storage::write_breadcrumb(&orphan).unwrap();

        assert_eq!(storage::active_count(), 1);

        // Abort should succeed (this was impossible in v0.2.x for orphans)
        let ctx = test_ctx();
        let result = abort("clearing stale orphan", Some("bc_1776327146_manager_dashboard_fix3"), &ctx);
        assert!(result.is_ok(), "Abort on migrated orphan should succeed: {:?}", result.err());
        assert_eq!(result.unwrap()["status"], "aborted");
        assert_eq!(storage::active_count(), 0);

        teardown_sandbox();
    }

    #[test]
    fn test_adopt() {
        let (_a, _c) = setup_sandbox();
        let ctx = test_ctx();

        let result = start("adopt test", vec!["s1".into()], None, &ctx).unwrap();
        let id = result["id"].as_str().unwrap();

        let new_ctx = WriterContext::new("new_actor", "machine2", "session2");
        let adopt_result = adopt(id, &new_ctx).unwrap();
        assert_eq!(adopt_result["new_owner"], "new_actor");
        assert_eq!(adopt_result["prev_owner"], "test_actor");

        teardown_sandbox();
    }

    #[test]
    fn test_read_active_index_compat() {
        let (_a, _c) = setup_sandbox();
        let ctx = test_ctx();

        start("compat test", vec!["s1".into()], None, &ctx).unwrap();

        let index = read_active_index();
        assert_eq!(index.len(), 1);
        let entry = index.values().next().unwrap();
        assert_eq!(entry.owner, "test_actor");

        teardown_sandbox();
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
