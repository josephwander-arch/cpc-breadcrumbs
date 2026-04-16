use serde::{Deserialize, Serialize};

/// Slugify a name for use in IDs and filenames.
/// Max `max_len` chars, lowercase, alphanumeric + `-_` only.
pub fn slugify(name: &str, max_len: usize) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_sep = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            slug.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            slug.push('_');
            last_sep = true;
        }
    }
    let trimmed = slug.trim_matches('_').to_string();
    let base = if trimmed.is_empty() {
        "operation".to_string()
    } else {
        trimmed
    };
    if base.len() > max_len {
        base[..max_len].trim_end_matches('_').to_string()
    } else {
        base
    }
}

/// Generate a breadcrumb ID: `bc_{unix_ts}_{slugified_name_40}`.
pub fn new_id(name: &str) -> String {
    let ts = chrono::Utc::now().timestamp();
    let slug = slugify(name, 40);
    format!("bc_{}_{}", ts, slug)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_idx: usize,
    pub step_name: String,
    pub result: String,
    pub at: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub other_session: String,
    pub other_at: String,
    pub this_session: String,
    pub this_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breadcrumb {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub owner: String,
    pub writer_actor: String,
    pub writer_machine: String,
    pub writer_session: String,
    pub writer_at: String,
    pub started_at: String,
    pub last_activity_at: String,
    pub steps: Vec<String>,
    pub current_step: usize,
    pub total_steps: usize,
    pub step_results: Vec<StepResult>,
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_warning: Option<ConflictInfo>,
    #[serde(default)]
    pub aborted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
    /// True if this breadcrumb was auto-started by a server (e.g. local auto-breadcrumb).
    #[serde(default)]
    pub auto_started: bool,
}

impl Breadcrumb {
    /// Compute whether this breadcrumb is stale (last_activity_at > 4h ago).
    pub fn is_stale(&self) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.last_activity_at)
            .map(|last| {
                let age = chrono::Utc::now()
                    .signed_duration_since(last.with_timezone(&chrono::Utc));
                age.num_hours() >= 4
            })
            .unwrap_or(false)
    }

    /// Return self with `stale` field populated.
    pub fn with_stale_computed(mut self) -> Self {
        self.stale = self.is_stale();
        self
    }

    /// Return current step name, or a fallback.
    pub fn current_step_name(&self) -> &str {
        self.steps
            .get(self.current_step)
            .map(|s| s.as_str())
            .unwrap_or("(beyond declared steps)")
    }
}

/// Lightweight entry stored in active.index.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub name: String,
    pub owner: String,
    pub last_activity_at: String,
    pub started_at: String,
}
