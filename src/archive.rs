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
