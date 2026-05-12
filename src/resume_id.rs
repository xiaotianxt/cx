#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitResumeId {
    #[cfg(feature = "service")]
    CxSession(crate::session::SessionId),
    AppThreadOrCodexSession(String),
}

impl ExplicitResumeId {
    pub(crate) fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        #[cfg(feature = "service")]
        {
            if let Ok(session_id) = crate::session::SessionId::parse(raw.clone()) {
                return Self::CxSession(session_id);
            }
        }
        Self::AppThreadOrCodexSession(raw)
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            #[cfg(feature = "service")]
            Self::CxSession(session_id) => session_id.as_str(),
            Self::AppThreadOrCodexSession(id) => id,
        }
    }
}
