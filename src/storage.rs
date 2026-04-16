use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

use fs2::FileExt;

use crate::error::BreadcrumbError;
use crate::schema::{Breadcrumb, IndexEntry};

// ── Path helpers ───────────────────────────────────────────────────────────────

pub fn state_dir() -> PathBuf {
    PathBuf::from(r"C:\CPC\state\breadcrumbs")
}

fn index_path() -> PathBuf {
    state_dir().join("active.index.json")
}

pub fn projects_dir() -> PathBuf {
    state_dir().join("projects")
}

pub fn project_file(project_id: &str) -> PathBuf {
    // Sanitize project_id to safe filename chars.
    let safe: String = project_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    projects_dir().join(format!("{}.jsonl", safe))
}

pub fn ensure_dirs() -> Result<(), BreadcrumbError> {
    std::fs::create_dir_all(state_dir()).map_err(BreadcrumbError::Io)?;
    std::fs::create_dir_all(projects_dir()).map_err(BreadcrumbError::Io)?;
    Ok(())
}

// ── Index (active.index.json) ─────────────────────────────────────────────────

/// Read the active index. Unlocked / best-effort. Returns empty map on failure.
pub fn read_index() -> HashMap<String, IndexEntry> {
    let path = index_path();
    if !path.exists() {
        return HashMap::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the active index atomically (write-tmp then rename).
pub fn write_index(index: &HashMap<String, IndexEntry>) -> Result<(), BreadcrumbError> {
    let path = index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(BreadcrumbError::Io)?;
    }
    let tmp = path.with_extension("tmp");
    let content = serde_json::to_string_pretty(index).map_err(BreadcrumbError::Serde)?;
    std::fs::write(&tmp, content).map_err(BreadcrumbError::Io)?;
    std::fs::rename(&tmp, &path).map_err(BreadcrumbError::Io)?;
    Ok(())
}

/// Add or update an entry in the index.
pub fn index_upsert(entry: IndexEntry) -> Result<(), BreadcrumbError> {
    let mut index = read_index();
    index.insert(entry.id.clone(), entry);
    write_index(&index)
}

/// Remove an entry from the index.
pub fn index_remove(id: &str) -> Result<(), BreadcrumbError> {
    let mut index = read_index();
    index.remove(id);
    write_index(&index)
}

// ── Project file (projects/{project_id}.jsonl) ─────────────────────────────────

/// Load all breadcrumbs from a project file (unlocked read).
pub fn load_project(project_id: &str) -> Vec<Breadcrumb> {
    let path = project_file(project_id);
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Breadcrumb>(line).ok())
        .collect()
}

/// Load all active breadcrumbs across all known projects (from index).
pub fn load_all_active() -> Vec<Breadcrumb> {
    let index = read_index();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut all: Vec<Breadcrumb> = Vec::new();

    for entry in index.values() {
        let pid = entry.project_id.as_deref().unwrap_or("_ungrouped");
        if seen.insert(pid.to_string()) {
            all.extend(load_project(pid));
        }
    }
    all
}

// ── Locking ───────────────────────────────────────────────────────────────────

/// Retry delays in ms — up to ~3s total.
const LOCK_RETRIES: &[u64] = &[100, 200, 400, 800, 1500];

/// Open a project file and acquire an exclusive lock, retrying up to ~3s.
/// Returns the locked file handle or a `ProjectLocked` error.
fn open_locked(path: &PathBuf, project_id: &str) -> Result<std::fs::File, BreadcrumbError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(BreadcrumbError::Io)?;

    for &delay_ms in LOCK_RETRIES {
        if file.try_lock_exclusive().is_ok() {
            return Ok(file);
        }
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    // One final attempt
    if file.try_lock_exclusive().is_ok() {
        return Ok(file);
    }
    Err(BreadcrumbError::ProjectLocked {
        project_id: project_id.to_string(),
        other_session: "unknown".to_string(),
    })
}

/// Read-modify-write a project file under exclusive lock.
/// `f` receives the current breadcrumbs and may mutate them.
pub fn locked_write_project<F>(
    project_id: &str,
    f: F,
) -> Result<(), BreadcrumbError>
where
    F: FnOnce(&mut Vec<Breadcrumb>) -> Result<(), BreadcrumbError>,
{
    ensure_dirs()?;
    let path = project_file(project_id);

    let mut file = open_locked(&path, project_id)?;

    // Read current content
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(BreadcrumbError::Io)?;

    let mut breadcrumbs: Vec<Breadcrumb> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Breadcrumb>(line).ok())
        .collect();

    // Apply mutation
    f(&mut breadcrumbs)?;

    // Overwrite: seek to start, truncate, write
    file.seek(SeekFrom::Start(0)).map_err(BreadcrumbError::Io)?;
    file.set_len(0).map_err(BreadcrumbError::Io)?;

    let mut writer = std::io::BufWriter::new(&file);
    for bc in &breadcrumbs {
        let line = serde_json::to_string(bc).map_err(BreadcrumbError::Serde)?;
        writeln!(writer, "{}", line).map_err(BreadcrumbError::Io)?;
    }
    writer.flush().map_err(BreadcrumbError::Io)?;
    // writer drops here, then file drops (releasing the lock)
    Ok(())
}

// ── Lookup helpers ────────────────────────────────────────────────────────────

/// Resolve which (breadcrumb_id, project_id) to operate on.
/// If `breadcrumb_id` is None, requires exactly 1 active; else ambiguity error.
pub fn resolve(
    breadcrumb_id: Option<&str>,
) -> Result<(String, String), BreadcrumbError> {
    let index = read_index();

    if let Some(id) = breadcrumb_id {
        return index
            .get(id)
            .map(|e| {
                let pid = e.project_id.as_deref().unwrap_or("_ungrouped").to_string();
                (id.to_string(), pid)
            })
            .ok_or_else(|| BreadcrumbError::NotFound { id: id.to_string() });
    }

    match index.len() {
        0 => Err(BreadcrumbError::NoActive),
        1 => {
            let (id, entry) = index.iter().next().unwrap();
            let pid = entry.project_id.as_deref().unwrap_or("_ungrouped").to_string();
            Ok((id.clone(), pid))
        }
        n => Err(BreadcrumbError::Ambiguous { count: n }),
    }
}

// ── Auto-reap ─────────────────────────────────────────────────────────────────

/// Reap breadcrumbs older than `hours` from all active projects.
/// Archives each one before removing from the active state.
/// Called on server startup if `CPC_BREADCRUMB_AUTO_REAP_HOURS` is set.
pub fn reap_stale(hours: u64) {
    let index = read_index();
    let now = chrono::Utc::now();
    let threshold = chrono::Duration::hours(hours as i64);
    let mut reaped_ids: Vec<String> = Vec::new();

    // Collect stale entries
    let mut stale_by_project: HashMap<String, Vec<String>> = HashMap::new();
    for (id, entry) in &index {
        let last = chrono::DateTime::parse_from_rfc3339(&entry.last_activity_at)
            .map(|dt| dt.with_timezone(&chrono::Utc));
        let is_stale = match last {
            Ok(dt) => now.signed_duration_since(dt) > threshold,
            Err(_) => false,
        };
        if is_stale {
            let pid = entry.project_id.as_deref().unwrap_or("_ungrouped").to_string();
            stale_by_project.entry(pid).or_default().push(id.clone());
        }
    }

    for (project_id, ids) in &stale_by_project {
        let reason = format!("auto-reaped: stale >{}h on server restart", hours);
        let res = locked_write_project(project_id, |breadcrumbs| {
            for id in ids {
                if let Some(bc) = breadcrumbs.iter_mut().find(|b| &b.id == id) {
                    bc.aborted = true;
                    bc.abort_reason = Some(reason.clone());
                    // Archive before removing
                    let _ = crate::archive::archive(bc);
                }
            }
            breadcrumbs.retain(|b| !ids.contains(&b.id));
            Ok(())
        });
        if res.is_ok() {
            reaped_ids.extend(ids.clone());
        } else {
            eprintln!("[breadcrumb reap] Failed to reap project {}: {:?}", project_id, res);
        }
    }

    if !reaped_ids.is_empty() {
        // Remove from index
        if let Ok(()) = (|| -> Result<(), BreadcrumbError> {
            let mut index = read_index();
            for id in &reaped_ids {
                index.remove(id);
            }
            write_index(&index)
        })() {
            eprintln!(
                "[breadcrumb reap] Reaped {} stale breadcrumb(s) (>{}h): {:?}",
                reaped_ids.len(),
                hours,
                reaped_ids
            );
        }
    }
}
