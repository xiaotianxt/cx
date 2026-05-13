#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitResumeId {
    AppThreadOrCodexSession(String),
}

impl ExplicitResumeId {
    pub(crate) fn parse(raw: impl Into<String>) -> Self {
        Self::AppThreadOrCodexSession(raw.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::AppThreadOrCodexSession(id) => id,
        }
    }
}
