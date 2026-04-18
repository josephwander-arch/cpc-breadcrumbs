use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::BreadcrumbError;
use crate::schema::{Breadcrumb, IndexEntry};

// ── Path helpers ───────────────────────────────────────────────────────────────

/// Active breadcrumb storage: one JSON file per breadcrumb.
fn active_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = test_helpers::get_active_dir_override() {
        return p;
    }
    volumes_breadcrumbs_base().join("active")
}

/// Completed breadcrumb archive: date-partitioned directories.
fn completed_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = test_helpers::get_completed_dir_override() {
        return p;
    }
    volumes_breadcrumbs_base().join("completed")
}

/// Resolve the Volumes breadcrumbs base path.
fn volumes_breadcrumbs_base() -> PathBuf {
    cpc_paths::volumes_path()
        .map(|v| v.join("breadcrumbs"))
        .unwrap_or_else(|_| PathBuf::from(r"C:\My Drive\Volumes\breadcrumbs"))
}

/// Legacy state directory (v0.2.x and earlier).
fn legacy_state_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = test_helpers::get_legacy_dir_override() {
        return p;
    }
    PathBuf::from(r"C:\CPC\state\breadcrumbs")
}

fn legacy_index_path() -> PathBuf {
    legacy_state_dir().join("active.index.json")
}

fn legacy_projects_dir() -> PathBuf {
    legacy_state_dir().join("projects")
}

pub fn ensure_dirs() -> Result<(), BreadcrumbError> {
    std::fs::create_dir_all(active_dir()).map_err(BreadcrumbError::Io)?;
    std::fs::create_dir_all(completed_dir()).map_err(BreadcrumbError::Io)?;
    Ok(())
}

// ── Active breadcrumb file helpers ─────────────────────────────────────────────

/// Path to a single active breadcrumb file.
fn active_file(id: &str) -> PathBuf {
    active_dir().join(format!("{}.json", id))
}

/// Atomic write: write to .tmp then rename. Last-writer-wins.
pub fn write_breadcrumb(bc: &Breadcrumb) -> Result<(), BreadcrumbError> {
    let path = active_file(&bc.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(BreadcrumbError::Io)?;
    }
    let tmp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(bc).map_err(BreadcrumbError::Serde)?;
    std::fs::write(&tmp, &content).map_err(BreadcrumbError::Io)?;
    std::fs::rename(&tmp, &path).map_err(BreadcrumbError::Io)?;
    Ok(())
}

/// Read a single breadcrumb by ID from active storage.
pub fn read_breadcrumb(id: &str) -> Result<Breadcrumb, BreadcrumbError> {
    let path = active_file(id);
    if !path.exists() {
        return Err(BreadcrumbError::NotFound { id: id.to_string() });
    }
    let content = std::fs::read_to_string(&path).map_err(BreadcrumbError::Io)?;
    serde_json::from_str(&content).map_err(BreadcrumbError::Serde)
}

/// Remove a breadcrumb from active storage (delete the file).
pub fn remove_active(id: &str) -> Result<(), BreadcrumbError> {
    let path = active_file(id);
    if path.exists() {
        std::fs::remove_file(&path).map_err(BreadcrumbError::Io)?;
    }
    Ok(())
}

/// Load all active breadcrumbs by reading every .json file in active/.
pub fn load_all_active() -> Vec<Breadcrumb> {
    let dir = active_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(bc) = serde_json::from_str::<Breadcrumb>(&content) {
                result.push(bc);
            }
        }
    }
    result
}

/// Count active breadcrumbs = count of .json files in active/.
pub fn active_count() -> usize {
    let dir = active_dir();
    std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0)
}

// ── Resolve ────────────────────────────────────────────────────────────────────

/// Resolve which breadcrumb_id to operate on.
/// If `breadcrumb_id` is None, requires exactly 1 active; else ambiguity error.
pub fn resolve(breadcrumb_id: Option<&str>) -> Result<String, BreadcrumbError> {
    if let Some(id) = breadcrumb_id {
        let path = active_file(id);
        if !path.exists() {
            return Err(BreadcrumbError::NotFound { id: id.to_string() });
        }
        return Ok(id.to_string());
    }

    let count = active_count();
    match count {
        0 => Err(BreadcrumbError::NoActive),
        1 => {
            // Find the single .json file
            let dir = active_dir();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            return Ok(stem.to_string());
                        }
                    }
                }
            }
            Err(BreadcrumbError::NoActive)
        }
        n => Err(BreadcrumbError::Ambiguous { count: n }),
    }
}

// ── Mutate helpers ─────────────────────────────────────────────────────────────

/// Read-modify-write a single breadcrumb. Atomic via tmp+rename.
pub fn mutate_breadcrumb<F>(id: &str, f: F) -> Result<Breadcrumb, BreadcrumbError>
where
    F: FnOnce(&mut Breadcrumb) -> Result<(), BreadcrumbError>,
{
    let mut bc = read_breadcrumb(id)?;
    f(&mut bc)?;
    write_breadcrumb(&bc)?;
    Ok(bc)
}

// ── Archive (complete/abort) ─────────────────────────────────────────────────

/// Move a breadcrumb from active/ to completed/{date}/.
/// Uses rename (same volume). Falls back to copy+delete on cross-device error.
pub fn archive_breadcrumb(bc: &Breadcrumb) -> Result<PathBuf, BreadcrumbError> {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dest_dir = completed_dir().join(&date);
    std::fs::create_dir_all(&dest_dir).map_err(BreadcrumbError::Io)?;

    let dest = dest_dir.join(format!("{}.json", bc.id));
    let content = serde_json::to_string_pretty(bc).map_err(BreadcrumbError::Serde)?;

    // Write the archive copy (atomic)
    let tmp = dest.with_extension("json.tmp");
    std::fs::write(&tmp, &content).map_err(BreadcrumbError::Io)?;
    std::fs::rename(&tmp, &dest).map_err(BreadcrumbError::Io)?;

    // Remove from active
    remove_active(&bc.id)?;

    Ok(dest)
}

/// Return the completed archive base path (for list/scan).
pub fn archive_base() -> PathBuf {
    completed_dir()
}

// ── Auto-reap ──────────────────────────────────────────────────────────────────

/// Reap breadcrumbs older than `hours` from active storage.
pub fn reap_stale(hours: u64) {
    let now = chrono::Utc::now();
    let threshold = chrono::Duration::hours(hours as i64);
    let all = load_all_active();
    let mut reaped: Vec<String> = Vec::new();

    for mut bc in all {
        let is_stale = chrono::DateTime::parse_from_rfc3339(&bc.last_activity_at)
            .map(|dt| now.signed_duration_since(dt.with_timezone(&chrono::Utc)) > threshold)
            .unwrap_or(false);

        if is_stale {
            bc.aborted = true;
            bc.abort_reason = Some(format!("auto-reaped: stale >{}h on server restart", hours));
            let _ = archive_breadcrumb(&bc);
            reaped.push(bc.id.clone());
        }
    }

    if !reaped.is_empty() {
        eprintln!(
            "[breadcrumb reap] Reaped {} stale breadcrumb(s) (>{}h): {:?}",
            reaped.len(),
            hours,
            reaped
        );
    }
}

// ── Legacy migration ───────────────────────────────────────────────────────────

/// Migrate from v0.2.x dual-store (index + JSONL) to v0.3.0 one-file-per-breadcrumb.
///
/// Called from init(). Idempotent — safe to run multiple times.
/// On success, renames the legacy dir to `breadcrumbs.migrated_{timestamp}`.
pub fn migrate_legacy() {
    let legacy_dir = legacy_state_dir();
    if !legacy_dir.exists() {
        return; // Nothing to migrate
    }

    // Check if there's actually data (index or project files)
    let has_index = legacy_index_path().exists();
    let has_projects = legacy_projects_dir().exists();
    if !has_index && !has_projects {
        return;
    }

    // Collect all breadcrumbs from all sources, deduplicating
    let mut all_breadcrumbs: HashMap<String, Breadcrumb> = HashMap::new();

    // 1. Read index entries (lightweight, used for dedup preference)
    let index_ids: std::collections::HashSet<String> = if has_index {
        std::fs::read_to_string(legacy_index_path())
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, IndexEntry>>(&s).ok())
            .map(|idx| idx.keys().cloned().collect())
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    // 2. Read ALL project JSONL files — this catches orphans
    if has_projects {
        if let Ok(entries) = std::fs::read_dir(legacy_projects_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for line in content.lines() {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if let Ok(bc) = serde_json::from_str::<Breadcrumb>(line) {
                            // JSONL entries are preferred (more recent writes)
                            all_breadcrumbs.insert(bc.id.clone(), bc);
                        }
                    }
                }
            }
        }
    }

    if all_breadcrumbs.is_empty() {
        return; // No data to migrate
    }

    let orphan_count = all_breadcrumbs
        .keys()
        .filter(|id| !index_ids.contains(id.as_str()))
        .count();

    // 3. Write each breadcrumb to the new active/ dir
    let mut migrated = 0usize;
    let mut errors = 0usize;
    for bc in all_breadcrumbs.values() {
        match write_breadcrumb(bc) {
            Ok(_) => migrated += 1,
            Err(e) => {
                eprintln!("[breadcrumb migrate] Failed to write {}: {}", bc.id, e);
                errors += 1;
            }
        }
    }

    if errors > 0 {
        eprintln!(
            "[breadcrumb migrate] {} errors during migration — legacy dir NOT renamed",
            errors
        );
        return;
    }

    // 4. Rename legacy dir (reversible — don't delete)
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let migrated_name = format!("breadcrumbs.migrated_{}", ts);
    let migrated_path = legacy_dir
        .parent()
        .unwrap_or(&legacy_dir)
        .join(migrated_name);
    match std::fs::rename(&legacy_dir, &migrated_path) {
        Ok(_) => {
            eprintln!(
                "[breadcrumb migrate] Migrated {} active breadcrumbs ({} orphans recovered) from legacy storage. Renamed to {}",
                migrated,
                orphan_count,
                migrated_path.display()
            );
        }
        Err(e) => {
            eprintln!(
                "[breadcrumb migrate] Migration data written but rename failed: {}. Legacy dir at {}",
                e,
                legacy_dir.display()
            );
        }
    }
}

// ── Test-only path overrides ───────────────────────────────────────────────────

#[cfg(test)]
pub mod test_helpers {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        static ACTIVE_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static COMPLETED_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static LEGACY_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub fn set_active_dir(path: PathBuf) {
        ACTIVE_DIR_OVERRIDE.with(|c| *c.borrow_mut() = Some(path));
    }
    pub fn set_completed_dir(path: PathBuf) {
        COMPLETED_DIR_OVERRIDE.with(|c| *c.borrow_mut() = Some(path));
    }
    pub fn set_legacy_dir(path: PathBuf) {
        LEGACY_DIR_OVERRIDE.with(|c| *c.borrow_mut() = Some(path));
    }
    pub fn clear_overrides() {
        ACTIVE_DIR_OVERRIDE.with(|c| *c.borrow_mut() = None);
        COMPLETED_DIR_OVERRIDE.with(|c| *c.borrow_mut() = None);
        LEGACY_DIR_OVERRIDE.with(|c| *c.borrow_mut() = None);
    }

    pub fn get_active_dir_override() -> Option<PathBuf> {
        ACTIVE_DIR_OVERRIDE.with(|c| c.borrow().clone())
    }
    pub fn get_completed_dir_override() -> Option<PathBuf> {
        COMPLETED_DIR_OVERRIDE.with(|c| c.borrow().clone())
    }
    pub fn get_legacy_dir_override() -> Option<PathBuf> {
        LEGACY_DIR_OVERRIDE.with(|c| c.borrow().clone())
    }
}
