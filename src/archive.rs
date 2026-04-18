use crate::error::BreadcrumbError;
use crate::schema::Breadcrumb;
use std::path::PathBuf;

/// Archive a breadcrumb: write to completed/{date}/ and remove from active/.
/// Delegates to storage::archive_breadcrumb.
pub fn archive(bc: &Breadcrumb) -> Result<PathBuf, BreadcrumbError> {
    crate::storage::archive_breadcrumb(bc)
}

/// Return the archive base path.
pub fn base() -> PathBuf {
    crate::storage::archive_base()
}
