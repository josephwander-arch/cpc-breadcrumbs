use thiserror::Error;

#[derive(Debug, Error)]
pub enum BreadcrumbError {
    #[error("No active breadcrumb")]
    NoActive,
    #[error("Ambiguous: {count} active breadcrumbs; provide breadcrumb_id to disambiguate")]
    Ambiguous { count: usize },
    #[error("Breadcrumb not found: {id}")]
    NotFound { id: String },
    #[error("Project '{project_id}' locked by {other_session}; retry in a moment")]
    ProjectLocked {
        project_id: String,
        other_session: String,
    },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for BreadcrumbError {
    fn from(e: anyhow::Error) -> Self {
        BreadcrumbError::Other(e.to_string())
    }
}
