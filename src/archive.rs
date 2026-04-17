use std::path::PathBuf;
use crate::schema::Breadcrumb;
use crate::error::BreadcrumbError;

/// Resolve the Volumes archive base path.
///
/// Uses cpc_paths::volumes_path() for the full resolution chain:
///   cache → VOLUMES_PATH env var → config → auto-detect → error
///
/// Falls back to the hardcoded Windows default if resolution fails,
/// so the breadcrumb system never hard-crashes due to path resolution.
fn archive_base() -> PathBuf {
    // Test override
    if let Ok(dir) = std::env::var("CPC_BREADCRUMB_ARCHIVE_DIR") {
        return PathBuf::from(dir);
    }
    cpc_paths::volumes_path()
        .map(|v| v.join("breadcrumbs").join("completed"))
        .unwrap_or_else(|_| PathBuf::from(r"C:\My Drive\Volumes\breadcrumbs\completed"))
}

/// Archive a breadcrumb to `{archive_base}/{YYYY-MM-DD}/bc_{id}.json`.
/// Called on complete or abort.
pub fn archive(bc: &Breadcrumb) -> Result<PathBuf, BreadcrumbError> {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dir = archive_base().join(&date);
    std::fs::create_dir_all(&dir).map_err(BreadcrumbError::Io)?;
    let filename = format!("{}.json", bc.id);
    let path = dir.join(&filename);
    let content = serde_json::to_string_pretty(bc).map_err(BreadcrumbError::Serde)?;
    std::fs::write(&path, content).map_err(BreadcrumbError::Io)?;
    Ok(path)
}

/// Return the archive base path.
pub fn base() -> PathBuf {
    archive_base()
}

/// Search completed archives for a breadcrumb by ID.
/// Checks today + last `days_back` days.
/// Returns (Breadcrumb, PathBuf) if found.
pub fn find_archived(id: &str, days_back: i64) -> Option<(crate::schema::Breadcrumb, PathBuf)> {
    let base = archive_base();
    let today = chrono::Local::now().date_naive();
    for offset in 0..=days_back {
        let date = today - chrono::Duration::days(offset);
        let dir = base.join(date.format("%Y-%m-%d").to_string());
        if !dir.exists() {
            continue;
        }
        // Try exact filename first
        let exact = dir.join(format!("{}.json", id));
        if exact.exists() {
            if let Ok(content) = std::fs::read_to_string(&exact) {
                if let Ok(bc) = serde_json::from_str::<crate::schema::Breadcrumb>(&content) {
                    if bc.id == id {
                        return Some((bc, exact));
                    }
                }
            }
        }
        // Fallback: scan directory for matching ID
        if let Ok(files) = std::fs::read_dir(&dir) {
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(bc) = serde_json::from_str::<crate::schema::Breadcrumb>(&content) {
                        if bc.id == id {
                            return Some((bc, path));
                        }
                    }
                }
            }
        }
    }
    None
}
