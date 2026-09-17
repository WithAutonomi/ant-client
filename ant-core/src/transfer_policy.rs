//! Native transfer failure policy, independent of runtime and error wording.

use crate::client_engine::adaptive::Outcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Timeout,
    Network,
    Application,
}

impl FailureKind {
    pub(crate) fn outcome(self) -> Outcome {
        match self {
            Self::Timeout => Outcome::Timeout,
            Self::Network => Outcome::NetworkError,
            Self::Application => Outcome::ApplicationError,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PutRejection {
    Full,
    PriceFloor,
    OtherRemote,
    Timeout,
    Dial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PutShortfall {
    ResponseTimeout,
    RemoteRejection,
    PeerChurn,
}

/// Native V2-554 policy: PUT-response timeouts signal capacity pressure;
/// dial churn and structured application rejections do not.
pub(crate) fn put_shortfall(
    timeout: usize,
    dial: usize,
    has_remote_rejection: bool,
) -> PutShortfall {
    if timeout > 0 {
        PutShortfall::ResponseTimeout
    } else if dial == 0 && has_remote_rejection {
        PutShortfall::RemoteRejection
    } else {
        PutShortfall::PeerChurn
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl PutShortfall {
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn failure_kind(self) -> FailureKind {
        match self {
            Self::ResponseTimeout => FailureKind::Network,
            Self::RemoteRejection | Self::PeerChurn => FailureKind::Application,
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, thiserror::Error)]
pub(crate) enum RpcError {
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Timeout(String),
    #[error("{code}: {message}")]
    Remote { code: String, message: String },
}

#[cfg(any(target_arch = "wasm32", test))]
impl From<String> for RpcError {
    fn from(message: String) -> Self {
        Self::Transport(message)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl RpcError {
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn put_rejection(&self) -> PutRejection {
        match self {
            Self::Timeout(_) => PutRejection::Timeout,
            Self::Transport(_) => PutRejection::Dial,
            Self::Remote { .. } => PutRejection::OtherRemote,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_shortfall_policy_is_independent_of_remote_error_text() {
        assert!(matches!(
            RpcError::Timeout("deadline expired".into()).put_rejection(),
            PutRejection::Timeout
        ));
        assert!(matches!(
            RpcError::Transport("deadline expired while connecting".into()).put_rejection(),
            PutRejection::Dial
        ));
        for message in [
            "price too low",
            "ICE failed",
            "request timed out",
            "storage full",
        ] {
            let rejection = RpcError::Remote {
                code: "put_failed".into(),
                message: message.into(),
            };
            assert!(matches!(
                rejection.put_rejection(),
                PutRejection::OtherRemote
            ));
            assert_eq!(
                put_shortfall(0, 0, true).failure_kind(),
                FailureKind::Application
            );
        }
        assert_eq!(
            put_shortfall(1, 0, true).failure_kind(),
            FailureKind::Network
        );
        assert_eq!(
            put_shortfall(0, 1, true).failure_kind(),
            FailureKind::Application
        );
        assert_eq!(put_shortfall(0, 0, false), PutShortfall::PeerChurn);
    }
}
