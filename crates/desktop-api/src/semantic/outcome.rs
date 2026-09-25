//! Action delivery is independent of task completion. This envelope also crosses
//! legacy string-only workflow callbacks without losing retry information.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    NotSent,
    Sent,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionEffect {
    Confirmed,
    Unverifiable,
    SuspectedNoop,
    Refused,
    Partial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopActionError {
    pub dispatch_state: DispatchState,
    pub effect: ActionEffect,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_allowed: Option<bool>,
}

impl DesktopActionError {
    const PREFIX: &'static str = "desktop_action_result:";

    pub fn encode(dispatch_state: DispatchState, message: impl Into<String>) -> String {
        let effect = match dispatch_state {
            DispatchState::NotSent => ActionEffect::Refused,
            DispatchState::Partial => ActionEffect::Partial,
            _ => ActionEffect::Unverifiable,
        };
        let error = Self {
            dispatch_state,
            effect,
            message: message.into(),
            retry_allowed: None,
        };
        format!(
            "{}{}",
            Self::PREFIX,
            serde_json::to_string(&error).expect("serializable error")
        )
    }

    pub fn decode(value: &str) -> Option<Self> {
        serde_json::from_str(value.strip_prefix(Self::PREFIX)?).ok()
    }

    pub fn refused(message: impl Into<String>) -> String {
        let error = Self {
            dispatch_state: DispatchState::NotSent,
            effect: ActionEffect::Refused,
            message: message.into(),
            retry_allowed: Some(false),
        };
        format!(
            "{}{}",
            Self::PREFIX,
            serde_json::to_string(&error).expect("serializable error")
        )
    }

    pub fn may_retry(&self) -> bool {
        self.dispatch_state == DispatchState::NotSent && self.retry_allowed != Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pre_dispatch_errors_are_retryable() {
        for state in [
            DispatchState::NotSent,
            DispatchState::Sent,
            DispatchState::Partial,
            DispatchState::Unknown,
        ] {
            let encoded = DesktopActionError::encode(state, "native timeout");
            let decoded = DesktopActionError::decode(&encoded).unwrap();
            assert_eq!(decoded.may_retry(), state == DispatchState::NotSent);
            assert_eq!(decoded.message, "native timeout");
        }
        assert!(DesktopActionError::decode("not a desktop result").is_none());
        assert!(
            !DesktopActionError::decode(&DesktopActionError::refused("user declined"))
                .unwrap()
                .may_retry()
        );
    }
}
