use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRole {
    Window,
    Button,
    TextField,
    CheckBox,
    RadioButton,
    List,
    ListItem,
    Menu,
    MenuItem,
    Tab,
    Document,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeAction {
    Activate,
    Invoke,
    Toggle,
    SetChecked,
    Select,
    Expand,
    Collapse,
    Focus,
    SetValue,
    Scroll,
    ScrollIntoView,
    SetRangeValue,
}

/// Legacy calls retain foreground delivery; new workflows explicitly choose Auto.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    #[default]
    Foreground,
    Auto,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIdentity {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowIdentity {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiNode {
    pub opaque_id: String,
    /// Stable, platform-neutral identity derived from the control's semantic
    /// locator, not its mutable value or traversal index. Native handles remain
    /// local. Older observations may omit this and use the legacy resolver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_key: Option<String>,
    pub role: UiRole,
    pub name: Option<String>,
    pub short_value: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub focused: bool,
    pub secure: bool,
    #[serde(default)]
    pub toggled: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub expanded: Option<bool>,
    /// Opaque hash used only to verify that a non-secret ValuePattern changed.
    #[serde(default)]
    pub value_fingerprint: Option<String>,
    pub supported_actions: Vec<NativeAction>,
}

/// Public, redacted observation. Native handles, process ids and geometry must
/// remain inside the platform adapter and must not be added here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub revision: u64,
    pub fingerprint: String,
    pub app: AppIdentity,
    pub window: WindowIdentity,
    pub nodes: Vec<UiNode>,
    pub captured_at_ms: u64,
    /// Missing nodes in a bounded/partial tree are not proof of disappearance.
    #[serde(default)]
    pub truncated: bool,
}

/// Local-only comparison of an input slot against a freshly observed value.
pub fn value_fingerprint(value: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("value:{:016x}", hasher.finish())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationScope {
    pub app_id: Option<String>,
    pub window_id: Option<String>,
    pub subtree_id: Option<String>,
    /// Local window handle supplied by target binding, never by a decision model.
    #[serde(skip)]
    pub window_handle: Option<i32>,
    #[serde(default)]
    pub delivery: DeliveryMode,
    /// Native traversal offset, distinct from response pagination.
    #[serde(default)]
    pub tree_offset: usize,
}

/// Stable semantic context for one ancestor in the accessibility tree.
///
/// The chain is ordered from the outermost retained ancestor to the direct
/// parent. Runtime ids, native handles, coordinates and observation indexes
/// are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<UiRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessible_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticLocator {
    pub app_id: String,
    /// Stable, locally-derived window identity when the platform exposes one.
    /// Older saved steps omit it and fall back to the optional title hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<UiRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessible_name: Option<String>,
    /// Stable row/container context used to distinguish repeated controls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestor_chain: Vec<SemanticContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_action: Option<NativeAction>,
    /// Legacy diagnostic hint retained for old workflow compatibility.
    /// Resolvers must never use it to break an ambiguous match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal_hint: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowedKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollAmount {
    Small,
    Page,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CandidateKind {
    ActivateWindow,
    LaunchKnownApp {
        app_id: String,
    },
    Invoke,
    Toggle,
    SetChecked {
        checked: bool,
    },
    ScrollIntoView,
    SetRangeValue {
        slot_id: String,
    },
    Select,
    Expand,
    Collapse,
    Focus,
    SetValue {
        slot_id: String,
    },
    SetSecret {
        slot_id: String,
    },
    PressKey {
        key: AllowedKey,
    },
    Scroll {
        direction: ScrollDirection,
        amount: ScrollAmount,
    },
    Wait {
        milliseconds: u32,
    },
    Done,
    AskUser,
    CannotProceed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    ReadOnly,
    Reversible,
    BoundedWrite,
    ExternalCommit,
    DestructiveCritical,
    Restricted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    pub name: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionCandidate {
    pub id: String,
    pub observation_revision: u64,
    pub target: Option<String>,
    pub kind: CandidateKind,
    pub public_description: String,
    pub local_risk: RiskClass,
    #[serde(default)]
    pub preconditions: Vec<Predicate>,
    #[serde(default)]
    pub expected_effects: Vec<Predicate>,
}

impl ActionCandidate {
    pub fn action_class(&self) -> ActionClass {
        match self.kind {
            CandidateKind::SetValue { .. } | CandidateKind::SetRangeValue { .. } => {
                ActionClass::SetValue
            }
            CandidateKind::SetSecret { .. } => ActionClass::SetSecret,
            CandidateKind::PressKey { .. } => ActionClass::PressKey,
            CandidateKind::Scroll { .. } => ActionClass::Scroll,
            CandidateKind::Wait { .. } => ActionClass::Wait,
            CandidateKind::Done | CandidateKind::AskUser | CandidateKind::CannotProceed => {
                ActionClass::Control
            }
            _ => ActionClass::NativeAction,
        }
    }

    pub fn slot_id(&self) -> Option<&str> {
        match &self.kind {
            CandidateKind::SetValue { slot_id }
            | CandidateKind::SetSecret { slot_id }
            | CandidateKind::SetRangeValue { slot_id } => Some(slot_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    NativeAction,
    SetValue,
    SetSecret,
    PressKey,
    Scroll,
    Wait,
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionGrant {
    pub workflow_id: String,
    pub workflow_version: String,
    pub capability_manifest_digest: String,
    pub allowed_apps: Vec<String>,
    pub action_classes: Vec<ActionClass>,
    #[serde(default)]
    pub secret_slot_ids: Vec<String>,
    pub visual_fallback: bool,
    pub unattended: bool,
    pub revoked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformCapability {
    ReadSemanticTree,
    NativeAction,
    WriteValue,
    ActivateWindow,
    RestrictedInput,
    Screenshot,
    VisualRecognition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    pub supported: Vec<PlatformCapability>,
    pub accessibility_permission: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionInput {
    pub goal: String,
    pub observation: Observation,
    pub candidates: Vec<ActionCandidate>,
    #[serde(default)]
    pub recent_actions: Vec<RecentAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentAction {
    pub action_class: ActionClass,
    pub target_summary: String,
    pub verification: Verification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// The only executable value a decision provider may emit.
    pub candidate_id: String,
    pub confidence: Option<f64>,
    #[serde(default)]
    pub probabilities: BTreeMap<String, f64>,
    pub actual_model: Option<String>,
    pub usage: Option<DecisionUsage>,
}

#[async_trait]
pub trait DecisionProvider: Send + Sync {
    async fn choose(&self, input: DecisionInput) -> Result<Decision, AutomationError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub candidate_id: String,
    pub dispatched: bool,
    /// Native background delivery or foreground input; never the requested Auto policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<DeliveryMode>,
    pub detail: Option<String>,
}

/// Ephemeral values supplied by the trusted local caller at dispatch time.
///
/// This payload is deliberately not serializable: decision providers select
/// only a candidate id and never receive the text that will be written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionInput {
    pub value: Option<String>,
    pub checked: Option<bool>,
    pub direction: Option<ScrollDirection>,
    pub amount: Option<ScrollAmount>,
    pub delivery: DeliveryMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Achieved,
    Progress,
    NoChange,
    Unexpected,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    NeedsIncrementalGrant(String),
    NeedsConfirmation(String),
    Deny(String),
}

pub trait Policy: Send + Sync {
    fn evaluate(
        &self,
        observation: &Observation,
        candidate: &ActionCandidate,
        grant: &ExecutionGrant,
    ) -> PolicyDecision;
}

/// Minimal deterministic policy shared by normal and enhanced mode.
pub struct LocalPolicy;

impl Policy for LocalPolicy {
    fn evaluate(
        &self,
        observation: &Observation,
        candidate: &ActionCandidate,
        grant: &ExecutionGrant,
    ) -> PolicyDecision {
        if grant.revoked {
            return PolicyDecision::Deny("execution grant was revoked".into());
        }
        if candidate.observation_revision != observation.revision {
            return PolicyDecision::Deny("candidate belongs to a stale observation".into());
        }
        if candidate.local_risk == RiskClass::Restricted {
            return PolicyDecision::Deny("restricted actions cannot be authorized".into());
        }
        if !grant
            .allowed_apps
            .iter()
            .any(|id| id == &observation.app.id)
        {
            return PolicyDecision::NeedsIncrementalGrant(format!(
                "application '{}' is outside the execution grant",
                observation.app.id
            ));
        }
        if !grant.action_classes.contains(&candidate.action_class()) {
            return PolicyDecision::NeedsIncrementalGrant(format!(
                "action class '{:?}' is outside the execution grant",
                candidate.action_class()
            ));
        }
        if let CandidateKind::SetSecret { slot_id } = &candidate.kind {
            if !grant.secret_slot_ids.contains(slot_id) {
                return PolicyDecision::NeedsIncrementalGrant(format!(
                    "secret slot '{slot_id}' is outside the execution grant"
                ));
            }
        }
        if candidate.local_risk == RiskClass::DestructiveCritical {
            return PolicyDecision::NeedsConfirmation(candidate.public_description.clone());
        }
        PolicyDecision::Allow
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceEventKind {
    Observed,
    CandidatesBuilt,
    DecisionMade,
    PolicyAllowed,
    Executed,
    Verified,
    Reconciled,
    SoftBudgetReached,
    StaleObservation,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub kind: TraceEventKind,
    pub observation_revision: Option<u64>,
    pub candidate_id: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    WaitingForUser,
    NeedsAttention,
    FailedSafely,
    BudgetExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub status: RunStatus,
    pub executed_steps: u32,
    pub trace: Vec<TraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    pub soft_max_steps: u32,
    pub hard_max_steps: u32,
    pub soft_max_elapsed_ms: u64,
    pub hard_max_elapsed_ms: u64,
    pub max_stall_count: u32,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            soft_max_steps: 20,
            hard_max_steps: 100,
            soft_max_elapsed_ms: 180_000,
            hard_max_elapsed_ms: 900_000,
            max_stall_count: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub goal: String,
    pub scope: ObservationScope,
    pub grant: ExecutionGrant,
    pub enhanced_mode: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AutomationError {
    #[error("observation failed: {0}")]
    Observation(String),
    #[error("candidate construction failed: {0}")]
    Candidates(String),
    #[error("decision failed: {0}")]
    Decision(String),
    #[error("decision selected unknown candidate id '{0}'")]
    UnknownCandidate(String),
    #[error("enhanced mode is unavailable: {0}")]
    EnhancedUnavailable(String),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("invalid protocol response: {0}")]
    Protocol(String),
}
