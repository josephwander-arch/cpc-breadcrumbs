use chrono::Utc;
use crate::schema::{Breadcrumb, ConflictInfo};

/// Check if there is a concurrent writer for this breadcrumb.
/// Returns `Some(ConflictInfo)` if `last_activity_at` is within 30s AND `writer_session` differs.
pub fn check(bc: &Breadcrumb, this_session: &str) -> Option<ConflictInfo> {
    if bc.writer_session == this_session {
        return None;
    }
    let last_at = chrono::DateTime::parse_from_rfc3339(&bc.last_activity_at).ok()?;
    let age = Utc::now().signed_duration_since(last_at.with_timezone(&Utc));
    if age.num_seconds().abs() <= 30 {
        Some(ConflictInfo {
            other_session: bc.writer_session.clone(),
            other_at: bc.last_activity_at.clone(),
            this_session: this_session.to_string(),
            this_at: Utc::now().to_rfc3339(),
        })
    } else {
        None
    }
}
