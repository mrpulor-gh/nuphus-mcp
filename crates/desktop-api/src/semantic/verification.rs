//! Read-only postconditions. A successful input call never proves these.
use super::types::{Observation, SemanticLocator, UiNode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionPolicy {
    #[default]
    Auto,
    Verified,
    Dispatched,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateCondition {
    Exists,
    Absent,
    WindowExists,
    WindowAbsent,
    Focused,
    Checked,
    Selected,
    Expanded,
    ValueEquals,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopExpectation {
    pub locator: SemanticLocator,
    pub condition: StateCondition,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default = "default_expected")]
    pub expected: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_samples")]
    pub stable_samples: u8,
}
fn default_expected() -> bool {
    true
}
fn default_timeout() -> u64 {
    5_000
}
fn default_samples() -> u8 {
    2
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateStatus {
    Satisfied,
    Unsatisfied,
    Unknown,
}

impl DesktopExpectation {
    pub fn validate(&self) -> Result<(), String> {
        if self.locator.app_id.trim().is_empty()
            || self.timeout_ms > 300_000
            || !(1..=5).contains(&self.stable_samples)
        {
            return Err("expectation 需要有效应用、0–300000ms 超时和 1–5 次稳定采样".into());
        }
        if self.condition == StateCondition::ValueEquals && self.value.is_none() {
            return Err("value_equals 需要 value".into());
        }
        if !matches!(
            self.condition,
            StateCondition::WindowExists | StateCondition::WindowAbsent
        ) && self.locator.role.is_none()
            && self.locator.accessible_name.is_none()
            && self.locator.automation_id.is_none()
        {
            return Err("元素后置条件需要稳定控件定位信息".into());
        }
        Ok(())
    }

    pub fn evaluate(&self, observation: &Observation, matches: &[&UiNode]) -> StateStatus {
        if observation.app.id != self.locator.app_id {
            return StateStatus::Unknown;
        }
        let expected_window = self
            .locator
            .window_id
            .as_ref()
            .is_none_or(|v| v == &observation.window.id)
            && self
                .locator
                .window_title
                .as_ref()
                .is_none_or(|v| v == &observation.window.title);
        if matches!(
            self.condition,
            StateCondition::WindowExists | StateCondition::WindowAbsent
        ) {
            return if self.condition == StateCondition::WindowExists && expected_window {
                StateStatus::Satisfied
            } else {
                // Observing a different window is not proof that the old one closed.
                StateStatus::Unknown
            };
        }
        if !expected_window {
            return StateStatus::Unknown;
        }
        if matches.len() > 1 {
            return StateStatus::Unknown;
        }
        let Some(node) = matches.first() else {
            return if observation.truncated {
                StateStatus::Unknown
            } else if self.condition == StateCondition::Absent {
                StateStatus::Satisfied
            } else {
                StateStatus::Unsatisfied
            };
        };
        let matched = match self.condition {
            StateCondition::Exists => Some(true),
            StateCondition::Absent => Some(false),
            StateCondition::Focused => Some(node.focused == self.expected),
            StateCondition::Checked => node.toggled.map(|v| v == self.expected),
            StateCondition::Selected => node.selected.map(|v| v == self.expected),
            StateCondition::Expanded => node.expanded.map(|v| v == self.expected),
            StateCondition::ValueEquals if !node.secure => node
                .value_fingerprint
                .as_ref()
                .zip(self.value.as_ref())
                .map(|(actual, expected)| {
                    let normalized;
                    let expected = if node
                        .supported_actions
                        .contains(&super::types::NativeAction::SetRangeValue)
                    {
                        let Ok(value) = expected.parse::<f64>() else {
                            return false;
                        };
                        if !value.is_finite() {
                            return false;
                        }
                        normalized = value.to_string();
                        &normalized
                    } else {
                        expected
                    };
                    actual == &super::types::value_fingerprint(expected)
                }),
            _ => None,
        };
        match matched {
            Some(true) => StateStatus::Satisfied,
            Some(false) => StateStatus::Unsatisfied,
            None => StateStatus::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::types::*;

    fn fixture(condition: StateCondition) -> (DesktopExpectation, Observation) {
        let expected = DesktopExpectation {
            locator: SemanticLocator {
                app_id: "app".into(),
                window_id: Some("window".into()),
                window_title: None,
                role: Some(UiRole::Button),
                automation_id: None,
                accessible_name: Some("Option".into()),
                ancestor_chain: vec![],
                supported_action: None,
                ordinal_hint: None,
            },
            condition,
            value: None,
            expected: true,
            timeout_ms: 1000,
            stable_samples: 2,
        };
        let observation = Observation {
            revision: 1,
            fingerprint: "state".into(),
            captured_at_ms: 0,
            truncated: false,
            app: AppIdentity {
                id: "app".into(),
                display_name: "Application".into(),
            },
            window: WindowIdentity {
                id: "window".into(),
                title: "Document".into(),
            },
            nodes: vec![UiNode {
                opaque_id: "node".into(),
                semantic_key: None,
                role: UiRole::Button,
                name: Some("Option".into()),
                short_value: None,
                enabled: true,
                visible: true,
                focused: false,
                secure: false,
                toggled: Some(true),
                selected: None,
                expanded: None,
                value_fingerprint: None,
                supported_actions: vec![],
            }],
        };
        (expected, observation)
    }

    #[test]
    fn absence_requires_complete_tree_and_unambiguous_scope() {
        let (expected, mut observation) = fixture(StateCondition::Absent);
        assert_eq!(expected.evaluate(&observation, &[]), StateStatus::Satisfied);
        observation.truncated = true;
        assert_eq!(expected.evaluate(&observation, &[]), StateStatus::Unknown);
        observation.truncated = false;
        observation.window.id = "other-document".into();
        assert_eq!(expected.evaluate(&observation, &[]), StateStatus::Unknown);
    }

    #[test]
    fn checked_state_distinguishes_false_mixed_and_duplicate_targets() {
        let (mut expected, mut observation) = fixture(StateCondition::Checked);
        assert_eq!(
            expected.evaluate(&observation, &[&observation.nodes[0]]),
            StateStatus::Satisfied
        );
        expected.expected = false;
        assert_eq!(
            expected.evaluate(&observation, &[&observation.nodes[0]]),
            StateStatus::Unsatisfied
        );
        observation.nodes[0].toggled = None;
        assert_eq!(
            expected.evaluate(&observation, &[&observation.nodes[0]]),
            StateStatus::Unknown
        );
        assert_eq!(
            expected.evaluate(
                &observation,
                &[&observation.nodes[0], &observation.nodes[0]]
            ),
            StateStatus::Unknown
        );
    }

    #[test]
    fn value_readback_preserves_text_but_normalizes_native_ranges() {
        let (mut expected, mut observation) = fixture(StateCondition::ValueEquals);
        expected.value = Some("3.0".into());
        observation.nodes[0].value_fingerprint = Some(value_fingerprint("3"));
        assert_eq!(
            expected.evaluate(&observation, &[&observation.nodes[0]]),
            StateStatus::Unsatisfied
        );
        observation.nodes[0].supported_actions = vec![NativeAction::SetRangeValue];
        assert_eq!(
            expected.evaluate(&observation, &[&observation.nodes[0]]),
            StateStatus::Satisfied
        );
        observation.nodes[0].secure = true;
        assert_eq!(
            expected.evaluate(&observation, &[&observation.nodes[0]]),
            StateStatus::Unknown
        );
    }

    #[test]
    fn window_absence_is_not_inferred_from_observing_a_different_window() {
        let (expected, mut observation) = fixture(StateCondition::WindowAbsent);
        observation.window.id = "other-window".into();
        assert_eq!(expected.evaluate(&observation, &[]), StateStatus::Unknown);
    }

    #[test]
    fn invalid_expectations_fail_before_dispatch() {
        let (mut expected, _) = fixture(StateCondition::ValueEquals);
        assert!(expected.validate().is_err());
        expected.value = Some("text".into());
        assert!(expected.validate().is_ok());
        expected.stable_samples = 0;
        assert!(expected.validate().is_err());
        expected.stable_samples = 1;
        expected.timeout_ms = 300_001;
        assert!(expected.validate().is_err());
    }
}
