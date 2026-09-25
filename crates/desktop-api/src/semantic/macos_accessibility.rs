//! macOS Accessibility adapter. Native references never leave the AX worker.
//!
//! Only capabilities actually advertised by a control become candidates. Saved
//! locators use bundle identifiers and semantic ancestry, not pid/AX pointers.

use super::types::*;
use super::{CandidateBuilder, ComputerExecutor, ComputerObserver};
use async_trait::async_trait;

#[cfg(any(target_os = "macos", test))]
mod semantic {
    use super::*;
    use std::hash::{Hash, Hasher};

    #[derive(Debug, Clone)]
    pub(super) struct Metadata {
        pub node: UiNode,
        pub identifier: Option<String>,
        pub raw_name: Option<String>,
        pub ancestors: Vec<SemanticContext>,
        pub origin: String,
        pub web_content: bool,
        pub scroll_directions: Vec<ScrollDirection>,
    }

    pub(super) const MENU_SCOPE_ID: &str = "nuphus:scope:menu";

    pub(super) fn scope_is_menu(ancestors: &[SemanticContext]) -> bool {
        ancestors
            .iter()
            .any(|context| context.automation_id.as_deref() == Some(MENU_SCOPE_ID))
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum ReplayResolution {
        Target,
        Region(String),
    }

    pub(super) fn replay_resolution(
        metadata: &[Metadata],
        locator: &SemanticLocator,
    ) -> Result<ReplayResolution, AutomationError> {
        let targets: Vec<_> = metadata
            .iter()
            .filter(|meta| {
                locator
                    .role
                    .as_ref()
                    .is_none_or(|role| role == &meta.node.role)
                    && locator
                        .automation_id
                        .as_ref()
                        .is_none_or(|id| Some(id) == meta.identifier.as_ref())
                    && locator
                        .accessible_name
                        .as_ref()
                        .is_none_or(|name| Some(name) == meta.raw_name.as_ref())
                    && (locator.ancestor_chain.is_empty()
                        || locator.ancestor_chain == meta.ancestors)
            })
            .collect();
        match targets.len() {
            1 => return Ok(ReplayResolution::Target),
            0 => {}
            _ => {
                return Err(AutomationError::Observation(
                    "saved AX target is ambiguous; refine its stable ancestor context".into(),
                ))
            }
        }
        // Prefer the deepest uniquely observed ancestor. Compare the preceding
        // chain as well, so a same-named group in another row is never selected.
        for (index, context) in locator.ancestor_chain.iter().enumerate().rev() {
            if context.automation_id.as_deref() == Some(MENU_SCOPE_ID)
                || (context.automation_id.is_none() && context.accessible_name.is_none())
            {
                continue;
            }
            let regions: Vec<_> = metadata
                .iter()
                .filter(|meta| {
                    meta.ancestors == locator.ancestor_chain[..index]
                        && context
                            .role
                            .as_ref()
                            .is_none_or(|role| role == &meta.node.role)
                        && context
                            .automation_id
                            .as_ref()
                            .is_none_or(|id| Some(id) == meta.identifier.as_ref())
                        && context
                            .accessible_name
                            .as_ref()
                            .is_none_or(|name| Some(name) == meta.raw_name.as_ref())
                })
                .collect();
            match regions.as_slice() {
                [region] => return Ok(ReplayResolution::Region(region.node.opaque_id.clone())),
                [] => {}
                _ => {
                    return Err(AutomationError::Observation(
                        "saved AX ancestor region is ambiguous; refine the workflow locator".into(),
                    ))
                }
            }
        }
        Err(AutomationError::Observation("saved AX target or its stable ancestor region is missing; refresh the workflow locator".into()))
    }

    pub(super) fn describe(meta: &Metadata, action: &NativeAction) -> String {
        let context = meta
            .ancestors
            .iter()
            .filter_map(|ancestor| ancestor.accessible_name.as_deref())
            .rev()
            .take(3)
            .map(|name| redact(name).chars().take(32).collect::<String>())
            .collect::<Vec<_>>();
        format!(
            "{} / {}: {action:?} {:?} '{}'",
            meta.origin,
            context.into_iter().rev().collect::<Vec<_>>().join(" > "),
            meta.node.role,
            meta.node.name.as_deref().unwrap_or("unnamed control")
        )
    }

    pub(super) fn descend_menu(
        raw_role: &str,
        explicit_root: bool,
        expanded: Option<bool>,
        selected: Option<bool>,
        focused: bool,
    ) -> bool {
        explicit_root
            || !matches!(raw_role, "AXMenuBarItem" | "AXMenuItem")
            || expanded == Some(true)
            || selected == Some(true)
            || focused
    }

    pub(super) fn ranked_actions(
        goal: &str,
        metadata: &[Metadata],
    ) -> Vec<(i32, Metadata, NativeAction)> {
        let goal = goal.to_lowercase();
        let mut ranked = Vec::new();
        for meta in metadata {
            if !meta.node.enabled {
                continue;
            }
            for action in &meta.node.supported_actions {
                if !meta.node.visible && *action != NativeAction::ScrollIntoView {
                    continue;
                }
                let duplicate_count = metadata
                    .iter()
                    .filter(|other| {
                        other.node.enabled
                            && (other.node.visible || *action == NativeAction::ScrollIntoView)
                            && other.node.role == meta.node.role
                            && other.identifier == meta.identifier
                            && other.raw_name == meta.raw_name
                            && other.ancestors == meta.ancestors
                            && other.origin == meta.origin
                            && other.node.supported_actions.contains(action)
                    })
                    .count();
                if duplicate_count != 1 {
                    continue;
                }
                let name = meta.node.name.as_deref().unwrap_or_default().to_lowercase();
                let context = meta
                    .ancestors
                    .iter()
                    .filter_map(|ancestor| ancestor.accessible_name.as_deref())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                let token_score: i32 = goal
                    .split(|ch: char| !ch.is_alphanumeric())
                    .filter(|token| token.chars().count() >= 2)
                    .map(|token| {
                        i32::from(name.contains(token)) * 40
                            + i32::from(context.contains(token)) * 15
                    })
                    .sum();
                let relevance = i32::from(!name.is_empty() && goal.contains(&name)) * 120
                    + token_score
                    + i32::from(meta.node.focused) * 30
                    + i32::from(matches!(
                        action,
                        NativeAction::SetValue | NativeAction::Invoke
                    )) * 18;
                ranked.push((relevance, meta.clone(), action.clone()));
            }
        }
        ranked.sort_by_key(|item| std::cmp::Reverse(item.0));
        ranked
    }

    pub(super) fn hash(value: &str) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    pub(super) fn check_window_binding(expected: u64, current: u64) -> Result<(), AutomationError> {
        if expected == current && expected != 0 {
            Ok(())
        } else {
            Err(AutomationError::Execution(
                "foreground AX window changed since the candidate was created".into(),
            ))
        }
    }

    pub(super) fn window_is_unique(
        enumeration_complete: bool,
        focused_enumerated: bool,
        matches: usize,
    ) -> bool {
        enumeration_complete && focused_enumerated && matches == 1
    }

    pub(super) fn check_deadline(deadline: std::time::Instant) -> Result<(), AutomationError> {
        if std::time::Instant::now() >= deadline {
            Err(AutomationError::Observation(
                "Accessibility observation timed out; partial tree discarded".into(),
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub(super) fn child_capacity(
        limit: usize,
        visited: usize,
        queued: usize,
        depth: usize,
    ) -> usize {
        if depth >= 24 {
            0
        } else {
            limit
                .saturating_sub(visited.saturating_add(queued).saturating_add(1))
                .min(200)
        }
    }

    pub(super) fn redact(value: &str) -> String {
        let lower = value.to_lowercase();
        if value.contains('@')
            || value
                .split(|c: char| !c.is_ascii_digit())
                .any(|part| part.len() >= 8)
            || ["apikey_", "api_key", "bearer ", "password", "密码"]
                .iter()
                .any(|s| lower.contains(s))
        {
            "redacted control".into()
        } else {
            value.chars().take(80).collect()
        }
    }

    pub(super) fn role(value: &str) -> UiRole {
        match value {
            "AXWindow" | "AXSheet" => UiRole::Window,
            "AXButton" | "AXPopUpButton" | "AXLink" => UiRole::Button,
            "AXTextField" | "AXTextArea" | "AXComboBox" => UiRole::TextField,
            "AXCheckBox" | "AXSwitch" => UiRole::CheckBox,
            "AXRadioButton" => UiRole::RadioButton,
            "AXList" | "AXOutline" | "AXTable" => UiRole::List,
            "AXRow" | "AXCell" => UiRole::ListItem,
            "AXMenu" | "AXMenuBar" => UiRole::Menu,
            "AXMenuItem" | "AXMenuBarItem" => UiRole::MenuItem,
            "AXTabGroup" => UiRole::Tab,
            "AXDocument" | "AXWebArea" => UiRole::Document,
            _ => UiRole::Other,
        }
    }

    /// Role/state parameters are inherent to accessibility action computation;
    /// mirrors the main workspace's clippy policy for this lint.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn actions(
        role: &UiRole,
        secure: bool,
        names: &[String],
        focused_settable: bool,
        value_settable: bool,
        selected_settable: bool,
        expanded_settable: bool,
        expanded: Option<bool>,
    ) -> Vec<NativeAction> {
        let mut result = Vec::new();
        if names.iter().any(|name| name == "AXPress") {
            result.push(match role {
                UiRole::CheckBox => NativeAction::Toggle,
                UiRole::RadioButton => NativeAction::Select,
                _ => NativeAction::Invoke,
            });
            if *role == UiRole::CheckBox {
                result.push(NativeAction::SetChecked);
            }
        }
        if selected_settable && !result.contains(&NativeAction::Select) {
            result.push(NativeAction::Select);
        }
        if *role == UiRole::CheckBox
            && value_settable
            && !result.contains(&NativeAction::SetChecked)
        {
            result.push(NativeAction::SetChecked);
        }
        if expanded_settable {
            match expanded {
                Some(true) => result.push(NativeAction::Collapse),
                Some(false) => result.push(NativeAction::Expand),
                None => {}
            }
        }
        if focused_settable {
            result.push(NativeAction::Focus);
        }
        // Values of password fields remain local and require the existing
        // explicit secret-input path, never an ordinary model value slot.
        if value_settable && !secure && *role == UiRole::TextField {
            result.push(NativeAction::SetValue);
        }
        if !scroll_directions(names).is_empty() {
            result.push(NativeAction::Scroll);
        }
        if names.iter().any(|name| name == "AXScrollToVisible") {
            result.push(NativeAction::ScrollIntoView);
        }
        result
    }

    pub(super) fn scroll_directions(names: &[String]) -> Vec<ScrollDirection> {
        [
            ScrollDirection::Up,
            ScrollDirection::Down,
            ScrollDirection::Left,
            ScrollDirection::Right,
        ]
        .into_iter()
        .filter(|direction| names.iter().any(|name| name == scroll_action(*direction)))
        .collect()
    }

    pub(super) fn scroll_action(direction: ScrollDirection) -> &'static str {
        match direction {
            ScrollDirection::Up => "AXScrollUpByPage",
            ScrollDirection::Down => "AXScrollDownByPage",
            ScrollDirection::Left => "AXScrollLeftByPage",
            ScrollDirection::Right => "AXScrollRightByPage",
        }
    }

    pub(super) fn checked_needs_press(
        current: Option<bool>,
        desired: bool,
    ) -> Result<bool, AutomationError> {
        current.map(|current| current != desired).ok_or_else(|| AutomationError::Execution(
            "checkbox state is mixed or unavailable; refresh or choose an explicit supported state action".into(),
        ))
    }

    pub(super) fn range_value(
        input: &str,
        minimum: f64,
        maximum: f64,
    ) -> Result<f64, AutomationError> {
        let value = input.parse::<f64>().ok().filter(|value| value.is_finite());
        value
            .filter(|value| {
                minimum.is_finite() && maximum.is_finite() && minimum <= *value && *value <= maximum
            })
            .ok_or_else(|| {
                AutomationError::Execution(
                    "range value must be finite and within the control's live minimum and maximum"
                        .into(),
                )
            })
    }

    pub(super) fn legacy_ax_enablement_allowed(status: i32) -> bool {
        status == -25205 // kAXErrorAttributeUnsupported, not timeout/app busy.
    }

    pub(super) fn native_child_page(total: usize, offset: usize, limit: usize) -> (usize, bool) {
        let remaining = total.saturating_sub(offset);
        let requested = remaining.min(limit).min(200);
        (requested, remaining > requested)
    }

    pub(super) fn same_process_lifetime(cached: Option<u64>, observed: Option<u64>) -> bool {
        observed.is_some() && cached == observed
    }

    #[cfg(test)]
    pub(super) fn kind(action: &NativeAction) -> Result<CandidateKind, AutomationError> {
        kind_with_input(action, &ExecutionInput::default())
    }

    pub(super) fn kind_with_input(
        action: &NativeAction,
        input: &ExecutionInput,
    ) -> Result<CandidateKind, AutomationError> {
        Ok(match action {
            NativeAction::Invoke => CandidateKind::Invoke,
            NativeAction::Toggle => CandidateKind::Toggle,
            NativeAction::SetChecked => CandidateKind::SetChecked {
                checked: input.checked.ok_or_else(|| {
                    AutomationError::Candidates(
                        "set_checked requires an explicit desired state".into(),
                    )
                })?,
            },
            NativeAction::ScrollIntoView => CandidateKind::ScrollIntoView,
            NativeAction::SetRangeValue => CandidateKind::SetRangeValue {
                slot_id: "value".into(),
            },
            NativeAction::Scroll => {
                if input.amount == Some(ScrollAmount::Small) {
                    return Err(AutomationError::Candidates("this AX control advertises page scrolling only; choose page or another local scroll route".into()));
                }
                CandidateKind::Scroll {
                    direction: input.direction.ok_or_else(|| {
                        AutomationError::Candidates("scroll requires an explicit direction".into())
                    })?,
                    amount: ScrollAmount::Page,
                }
            }
            NativeAction::Select => CandidateKind::Select,
            NativeAction::Expand => CandidateKind::Expand,
            NativeAction::Collapse => CandidateKind::Collapse,
            NativeAction::Focus => CandidateKind::Focus,
            NativeAction::SetValue => CandidateKind::SetValue {
                slot_id: "value".into(),
            },
            _ => return Err(AutomationError::Candidates("unsupported AX action".into())),
        })
    }

    pub(super) fn risk(action: &NativeAction, name: Option<&str>) -> RiskClass {
        crate::semantic::classify_desktop_risk(action, name)
    }

    pub(super) fn matches(
        meta: &Metadata,
        locator: &SemanticLocator,
        action: &NativeAction,
    ) -> bool {
        meta.node.enabled
            && (meta.node.visible || *action == NativeAction::ScrollIntoView)
            && locator.role.as_ref().is_none_or(|r| r == &meta.node.role)
            && locator
                .automation_id
                .as_ref()
                .is_none_or(|id| Some(id) == meta.identifier.as_ref())
            && locator
                .accessible_name
                .as_ref()
                .is_none_or(|name| Some(name) == meta.raw_name.as_ref())
            && (locator.ancestor_chain.is_empty() || locator.ancestor_chain == meta.ancestors)
            && meta.node.supported_actions.contains(action)
    }

    pub(super) fn effects(action: &NativeAction, target: &str) -> Vec<Predicate> {
        let name = match action {
            NativeAction::Invoke => "invoke_target_changed_or_disappeared",
            NativeAction::Toggle => "target_toggle_changed",
            NativeAction::SetChecked => "target_checked",
            NativeAction::Scroll => "target_scrolled",
            NativeAction::ScrollIntoView => "target_visible",
            NativeAction::SetRangeValue => "target_range_value",
            NativeAction::Select => "target_selected",
            NativeAction::Expand => "target_expanded",
            NativeAction::Collapse => "target_collapsed",
            NativeAction::Focus => "target_focused",
            NativeAction::SetValue => "target_value_changed",
            _ => return vec![],
        };
        vec![Predicate {
            name: name.into(),
            arguments: [("target".into(), target.into())].into(),
        }]
    }

    pub(super) fn metadata_effects(meta: &Metadata, action: &NativeAction) -> Vec<Predicate> {
        let mut predicates = effects(action, &meta.node.opaque_id);
        for predicate in &mut predicates {
            predicate.arguments.insert(
                "readback_trust".into(),
                if meta.web_content {
                    "web_content"
                } else {
                    "native"
                }
                .into(),
            );
        }
        predicates
    }

    pub(super) fn requires_foreground(
        delivery: DeliveryMode,
        bound_window: bool,
        action: &NativeAction,
    ) -> bool {
        !bound_window || delivery == DeliveryMode::Foreground || *action == NativeAction::Focus
    }

    pub(super) fn parameter_inputs(meta: &Metadata, action: &NativeAction) -> Vec<ExecutionInput> {
        match action {
            NativeAction::SetChecked => [true, false]
                .into_iter()
                .map(|checked| ExecutionInput {
                    checked: Some(checked),
                    ..Default::default()
                })
                .collect(),
            NativeAction::Scroll => meta
                .scroll_directions
                .iter()
                .map(|direction| ExecutionInput {
                    direction: Some(*direction),
                    amount: Some(ScrollAmount::Page),
                    ..Default::default()
                })
                .collect(),
            _ => vec![ExecutionInput::default()],
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod native;

#[cfg(target_os = "macos")]
mod platform {
    use super::semantic::*;
    use super::*;
    use std::collections::HashMap;
    use std::sync::{mpsc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use tokio::sync::oneshot;

    #[derive(Clone)]
    struct BoundAction {
        candidate: ActionCandidate,
        locator: SemanticLocator,
        action: NativeAction,
        window_token: u64,
        require_unique_window: bool,
        scope: ObservationScope,
    }

    #[derive(Default)]
    struct State {
        revision: u64,
        fingerprint: String,
        observation: Option<Observation>,
        metadata: Vec<Metadata>,
        actions: HashMap<String, BoundAction>,
        window_token: u64,
        window_unique: bool,
        scope: ObservationScope,
    }

    enum Request {
        Observe(
            ObservationScope,
            Instant,
            oneshot::Sender<Result<native::Snapshot, AutomationError>>,
        ),
        Execute(
            Box<BoundAction>,
            ExecutionInput,
            Instant,
            oneshot::Sender<Result<ActionReceipt, AutomationError>>,
        ),
    }

    pub struct MacosAccessibilityAdapter {
        worker: mpsc::SyncSender<Request>,
        state: Mutex<State>,
    }

    impl Default for MacosAccessibilityAdapter {
        fn default() -> Self {
            Self::new(200)
        }
    }

    impl MacosAccessibilityAdapter {
        pub fn new(max_elements: usize) -> Self {
            let (sender, receiver) = mpsc::sync_channel(1);
            // A failed spawn disconnects the sender; observe then reports a
            // regular adapter error instead of crashing the application.
            let _ = std::thread::Builder::new()
                .name("nuphus-ax".into())
                .spawn(move || {
                    let mut session = native::Session::default();
                    while let Ok(request) = receiver.recv() {
                        match request {
                            Request::Observe(scope, deadline, reply) => {
                                if !reply.is_closed() {
                                    let result = session.capture(
                                        max_elements.clamp(1, 200),
                                        &scope,
                                        deadline,
                                    );
                                    let _ = reply.send(result);
                                }
                            }
                            Request::Execute(mut bound, input, deadline, reply) => {
                                if !reply.is_closed() {
                                    bound.scope.delivery = input.delivery;
                                    let result = session
                                        .execute(
                                            max_elements.clamp(1, 200),
                                            bound.window_token,
                                            bound.require_unique_window,
                                            &bound.scope,
                                            &bound.locator,
                                            &bound.action,
                                            &input,
                                            deadline,
                                            || reply.is_closed(),
                                        )
                                        .map(|dispatched| ActionReceipt {
                                            candidate_id: bound.candidate.id,
                                            dispatched,
                                            delivery_mode: Some(if requires_foreground(input.delivery, bound.scope.window_handle.is_some(), &bound.action) { DeliveryMode::Foreground } else { DeliveryMode::Background }),
                                            detail: Some(if dispatched { "macOS Accessibility action dispatched" } else { "macOS Accessibility target already has the requested state" }.into()),
                                        });
                                    let _ = reply.send(result);
                                }
                            }
                        }
                    }
                });
            Self {
                worker: sender,
                state: Mutex::new(State::default()),
            }
        }

        fn bind(
            state: &mut State,
            observation: &Observation,
            meta: &Metadata,
            action: NativeAction,
            input: &ExecutionInput,
        ) -> Result<ActionCandidate, AutomationError> {
            let locator = SemanticLocator {
                app_id: observation.app.id.clone(),
                window_id: Some(observation.window.id.clone()),
                window_title: Some(observation.window.title.clone()),
                role: Some(meta.node.role.clone()),
                automation_id: meta.identifier.clone(),
                accessible_name: meta.raw_name.clone(),
                ancestor_chain: meta.ancestors.clone(),
                supported_action: Some(action.clone()),
                ordinal_hint: None,
            };
            let expected_effects = metadata_effects(meta, &action);
            let kind = kind_with_input(&action, input)?;
            if let CandidateKind::Scroll { direction, .. } = &kind {
                if !meta.scroll_directions.contains(direction) {
                    return Err(AutomationError::Candidates(
                        "the target does not advertise this AX scroll direction".into(),
                    ));
                }
            }
            let candidate = ActionCandidate {
                id: format!("ax:{}", uuid::Uuid::new_v4().simple()),
                observation_revision: observation.revision,
                target: Some(meta.node.opaque_id.clone()),
                public_description: format!("{} ({kind:?})", describe(meta, &action)),
                kind,
                local_risk: risk(&action, meta.raw_name.as_deref()),
                preconditions: vec![Predicate {
                    name: "semantic_element_exists".into(),
                    arguments: [("target".into(), meta.node.opaque_id.clone())].into(),
                }],
                expected_effects,
            };
            state.actions.insert(
                candidate.id.clone(),
                BoundAction {
                    candidate: candidate.clone(),
                    locator,
                    action,
                    window_token: state.window_token,
                    require_unique_window: false,
                    scope: state.scope.clone(),
                },
            );
            Ok(candidate)
        }

        fn current(state: &State, observation: &Observation) -> Result<(), AutomationError> {
            if state.observation.as_ref() != Some(observation) {
                return Err(AutomationError::Candidates(
                    "AX observation is stale or was not created by this adapter".into(),
                ));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl ComputerObserver for MacosAccessibilityAdapter {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities {
                supported: vec![
                    PlatformCapability::ReadSemanticTree,
                    PlatformCapability::NativeAction,
                    PlatformCapability::WriteValue,
                ],
                accessibility_permission: native::trusted(),
            }
        }

        async fn observe(&self, scope: &ObservationScope) -> Result<Observation, AutomationError> {
            let (reply, receive) = oneshot::channel();
            self.worker
                .try_send(Request::Observe(
                    scope.clone(),
                    Instant::now() + Duration::from_secs(5),
                    reply,
                ))
                .map_err(|_| {
                    AutomationError::Observation("AX worker is busy or unavailable".into())
                })?;
            let snapshot = tokio::time::timeout(Duration::from_secs(6), receive)
                .await
                .map_err(|_| AutomationError::Observation("AX observation timed out".into()))?
                .map_err(|_| AutomationError::Observation("AX worker stopped".into()))??;
            let mut state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Observation("AX state unavailable".into()))?;
            if state.fingerprint != snapshot.fingerprint {
                state.revision = state.revision.saturating_add(1).max(1);
                state.fingerprint = snapshot.fingerprint.clone();
            }
            let observation = Observation {
                revision: state.revision,
                fingerprint: snapshot.fingerprint,
                app: snapshot.app,
                window: snapshot.window,
                nodes: snapshot.metadata.iter().map(|m| m.node.clone()).collect(),
                truncated: snapshot.truncated,
                captured_at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            state.metadata = snapshot.metadata;
            state.window_token = snapshot.window_token;
            state.window_unique = snapshot.window_unique;
            state.scope = scope.clone();
            state.observation = Some(observation.clone());
            Ok(observation)
        }

        async fn observe_locator(
            &self,
            locator: &SemanticLocator,
        ) -> Result<(Observation, ObservationScope), AutomationError> {
            self.observe_locator_scoped(locator, &ObservationScope::default())
                .await
        }

        async fn observe_locator_scoped(
            &self,
            locator: &SemanticLocator,
            requested: &ObservationScope,
        ) -> Result<(Observation, ObservationScope), AutomationError> {
            let mut scope = ObservationScope {
                app_id: Some(locator.app_id.clone()),
                window_id: locator.window_id.clone(),
                subtree_id: scope_is_menu(&locator.ancestor_chain).then(|| "@menu".into()),
                window_handle: requested.window_handle,
                delivery: requested.delivery,
                tree_offset: requested.tree_offset,
            };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            let mut visited = std::collections::HashSet::new();
            for _ in 0..32 {
                let observation = tokio::time::timeout_at(deadline, self.observe(&scope))
                    .await
                    .map_err(|_| {
                        AutomationError::Observation("saved AX region resolution timed out".into())
                    })??;
                native::validate_locator_window(locator, &observation)?;
                let resolution = {
                    let state = self
                        .state
                        .lock()
                        .map_err(|_| AutomationError::Observation("AX state unavailable".into()))?;
                    Self::current(&state, &observation)?;
                    if !state.window_unique {
                        return Err(AutomationError::Observation("saved AX window identity is ambiguous; select a unique window before replay".into()));
                    }
                    replay_resolution(&state.metadata, locator)
                };
                let resolution = match resolution {
                    Ok(value) => value,
                    Err(error) if observation.truncated && scope.tree_offset < 10_000 => {
                        if error.to_string().contains("ambiguous") {
                            return Err(error);
                        }
                        if observation.nodes.is_empty() {
                            return Err(error);
                        }
                        scope.tree_offset += observation.nodes.len();
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                match resolution {
                    ReplayResolution::Target => return Ok((observation, scope)),
                    ReplayResolution::Region(id) => {
                        if !visited.insert(id.clone())
                            || scope.subtree_id.as_deref() == Some(id.as_str())
                        {
                            return Err(AutomationError::Observation("saved AX target was not found in its ancestor region; refresh the workflow locator".into()));
                        }
                        scope.subtree_id = Some(id);
                        scope.tree_offset = 0;
                    }
                }
            }
            Err(AutomationError::Observation(
                "saved AX ancestor resolution exceeded its bounded depth".into(),
            ))
        }
    }

    impl CandidateBuilder for MacosAccessibilityAdapter {
        fn node_locator(
            &self,
            observation: &Observation,
            node: &UiNode,
        ) -> Option<SemanticLocator> {
            let state = self.state.lock().ok()?;
            Self::current(&state, observation).ok()?;
            let meta = state
                .metadata
                .iter()
                .find(|meta| meta.node.opaque_id == node.opaque_id)?;
            Some(SemanticLocator {
                app_id: observation.app.id.clone(),
                window_id: Some(observation.window.id.clone()),
                window_title: Some(observation.window.title.clone()),
                role: Some(node.role.clone()),
                automation_id: meta.identifier.clone(),
                accessible_name: meta.raw_name.clone(),
                ancestor_chain: meta.ancestors.clone(),
                supported_action: None,
                ordinal_hint: None,
            })
        }

        fn value_readback_reliable(&self, node: &UiNode) -> bool {
            self.state
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .metadata
                        .iter()
                        .find(|meta| meta.node.opaque_id == node.opaque_id)
                        .map(|meta| !meta.web_content)
                })
                .unwrap_or(false)
        }

        fn build(
            &self,
            goal: &str,
            observation: &Observation,
        ) -> Result<Vec<ActionCandidate>, AutomationError> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Candidates("AX state unavailable".into()))?;
            Self::current(&state, observation)?;
            let ranked = ranked_actions(goal, &state.metadata);
            state.actions.clear();
            let mut result = Vec::new();
            for (_, meta, action) in ranked {
                for input in parameter_inputs(&meta, &action) {
                    result.push(Self::bind(
                        &mut state,
                        observation,
                        &meta,
                        action.clone(),
                        &input,
                    )?);
                }
            }
            for (kind, description) in [
                (CandidateKind::Done, "The bounded goal is complete"),
                (
                    CandidateKind::AskUser,
                    "Ask the user for information needed to continue",
                ),
                (
                    CandidateKind::CannotProceed,
                    "No offered semantic action can advance the goal",
                ),
            ] {
                result.push(ActionCandidate {
                    id: format!("ax:{}", uuid::Uuid::new_v4().simple()),
                    observation_revision: observation.revision,
                    target: None,
                    kind,
                    public_description: description.into(),
                    local_risk: RiskClass::ReadOnly,
                    preconditions: vec![],
                    expected_effects: vec![],
                });
            }
            Ok(result)
        }

        fn semantic_locator(&self, candidate: &ActionCandidate) -> Option<SemanticLocator> {
            let state = self.state.lock().ok()?;
            let bound = state.actions.get(&candidate.id)?;
            (bound.candidate == *candidate).then(|| bound.locator.clone())
        }

        fn rebuild_semantic_candidate(
            &self,
            locator: &SemanticLocator,
            action: NativeAction,
            observation: &Observation,
        ) -> Result<ActionCandidate, AutomationError> {
            self.rebuild_semantic_candidate_with_input(
                locator,
                action,
                observation,
                &ExecutionInput::default(),
            )
        }

        fn rebuild_semantic_candidate_with_input(
            &self,
            locator: &SemanticLocator,
            action: NativeAction,
            observation: &Observation,
            input: &ExecutionInput,
        ) -> Result<ActionCandidate, AutomationError> {
            kind_with_input(&action, input)?;
            native::validate_locator_window(locator, observation)?;
            if locator
                .supported_action
                .as_ref()
                .is_some_and(|a| a != &action)
            {
                return Err(AutomationError::Candidates(
                    "saved action does not match AX locator capability".into(),
                ));
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Candidates("AX state unavailable".into()))?;
            Self::current(&state, observation)?;
            if scope_is_menu(&locator.ancestor_chain)
                && !state.metadata.iter().any(|meta| meta.origin == "menu")
            {
                return Err(AutomationError::Candidates("saved menu action requires an @menu observation or a menu region; refresh that scope before replay".into()));
            }
            if !state.window_unique {
                return Err(AutomationError::Candidates("saved AX window identity is ambiguous; give the windows distinct titles or close the duplicate before replay".into()));
            }
            let found: Vec<_> = state
                .metadata
                .iter()
                .filter(|m| matches(m, locator, &action))
                .cloned()
                .collect();
            let [meta] = found.as_slice() else {
                return Err(AutomationError::Candidates("AX locator is missing or ambiguous; refresh the observation or specify ancestor context".into()));
            };
            let candidate = Self::bind(&mut state, observation, meta, action, input)?;
            if let Some(bound) = state.actions.get_mut(&candidate.id) {
                bound.require_unique_window = true;
            }
            Ok(candidate)
        }
    }

    #[async_trait]
    impl ComputerExecutor for MacosAccessibilityAdapter {
        async fn execute(
            &self,
            fresh: &Observation,
            action: &ActionCandidate,
            input: &ExecutionInput,
        ) -> Result<ActionReceipt, AutomationError> {
            let bound = {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| AutomationError::Execution("AX state unavailable".into()))?;
                Self::current(&state, fresh)?;
                let bound = state
                    .actions
                    .get(&action.id)
                    .filter(|bound| &bound.candidate == action)
                    .ok_or_else(|| {
                        AutomationError::Execution(
                            "candidate was not created by this AX adapter or was modified".into(),
                        )
                    })?;
                check_window_binding(bound.window_token, state.window_token)?;
                bound.clone()
            };
            let mut input = input.clone();
            match &bound.candidate.kind {
                CandidateKind::SetChecked { checked } => input.checked = Some(*checked),
                CandidateKind::Scroll { direction, amount } => {
                    input.direction = Some(*direction);
                    input.amount = Some(*amount);
                }
                _ => {}
            }
            let (reply, receive) = oneshot::channel();
            self.worker
                .try_send(Request::Execute(
                    Box::new(bound),
                    input,
                    Instant::now() + Duration::from_secs(5),
                    reply,
                ))
                .map_err(|_| {
                    AutomationError::Execution("AX worker is busy or unavailable".into())
                })?;
            tokio::time::timeout(Duration::from_secs(6), receive)
                .await
                .map_err(|_| {
                    AutomationError::Execution(
                        "AX action timed out; reobserve before retrying".into(),
                    )
                })?
                .map_err(|_| AutomationError::Execution("AX worker stopped".into()))?
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::MacosAccessibilityAdapter;

#[cfg(not(target_os = "macos"))]
#[derive(Default)]
pub struct MacosAccessibilityAdapter;

#[cfg(not(target_os = "macos"))]
#[async_trait]
impl ComputerObserver for MacosAccessibilityAdapter {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            supported: vec![],
            accessibility_permission: false,
        }
    }
    async fn observe(&self, _: &ObservationScope) -> Result<Observation, AutomationError> {
        Err(AutomationError::Observation(
            "macOS Accessibility requires macOS".into(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
impl CandidateBuilder for MacosAccessibilityAdapter {
    fn build(&self, _: &str, _: &Observation) -> Result<Vec<ActionCandidate>, AutomationError> {
        Err(AutomationError::Candidates(
            "macOS Accessibility requires macOS".into(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
#[async_trait]
impl ComputerExecutor for MacosAccessibilityAdapter {
    async fn execute(
        &self,
        _: &Observation,
        _: &ActionCandidate,
        _: &ExecutionInput,
    ) -> Result<ActionReceipt, AutomationError> {
        Err(AutomationError::Execution(
            "macOS Accessibility requires macOS".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::semantic::*;
    use super::*;

    fn metadata(container: &str) -> Metadata {
        Metadata {
            node: UiNode {
                opaque_id: "axe:button".into(),
                semantic_key: Some("ax:button".into()),
                role: UiRole::Button,
                name: Some("Open".into()),
                short_value: None,
                enabled: true,
                visible: true,
                focused: false,
                secure: false,
                toggled: None,
                selected: None,
                expanded: None,
                value_fingerprint: None,
                supported_actions: vec![NativeAction::Invoke],
            },
            identifier: Some("open".into()),
            raw_name: Some("Open".into()),
            ancestors: vec![SemanticContext {
                role: Some(UiRole::ListItem),
                automation_id: Some(container.into()),
                accessible_name: None,
            }],
            origin: "window".into(),
            web_content: false,
            scroll_directions: vec![],
        }
    }

    #[test]
    fn capabilities_are_advertised_only_when_supported_by_ax() {
        assert!(actions(
            &UiRole::Button,
            false,
            &[],
            false,
            false,
            false,
            false,
            None
        )
        .is_empty());
        assert_eq!(
            actions(
                &UiRole::CheckBox,
                false,
                &["AXPress".into()],
                false,
                false,
                false,
                false,
                None
            ),
            vec![NativeAction::Toggle, NativeAction::SetChecked]
        );
        assert_eq!(
            actions(
                &UiRole::RadioButton,
                false,
                &["AXPress".into()],
                false,
                false,
                true,
                false,
                None
            ),
            vec![NativeAction::Select]
        );
        assert_eq!(
            actions(
                &UiRole::ListItem,
                false,
                &[],
                false,
                false,
                false,
                true,
                Some(true)
            ),
            vec![NativeAction::Collapse]
        );
        assert_eq!(
            actions(
                &UiRole::ListItem,
                false,
                &[],
                false,
                false,
                false,
                true,
                Some(false)
            ),
            vec![NativeAction::Expand]
        );
    }

    #[test]
    fn secure_fields_and_read_only_controls_never_offer_ordinary_set_value() {
        assert_eq!(
            actions(
                &UiRole::TextField,
                true,
                &[],
                true,
                true,
                false,
                false,
                None
            ),
            vec![NativeAction::Focus]
        );
        assert!(actions(
            &UiRole::TextField,
            false,
            &[],
            false,
            false,
            false,
            false,
            None
        )
        .is_empty());
        assert_eq!(
            actions(
                &UiRole::TextField,
                false,
                &[],
                false,
                true,
                false,
                false,
                None
            ),
            vec![NativeAction::SetValue]
        );
        assert!(actions(&UiRole::Other, false, &[], false, true, false, false, None).is_empty());
    }

    #[test]
    fn repeated_controls_resolve_using_semantic_ancestry_not_ordinal() {
        let first = metadata("row-first");
        let second = metadata("row-second");
        let locator = SemanticLocator {
            app_id: "com.apple.finder".into(),
            window_id: None,
            window_title: None,
            role: Some(UiRole::Button),
            automation_id: first.identifier.clone(),
            accessible_name: first.raw_name.clone(),
            ancestor_chain: first.ancestors.clone(),
            supported_action: Some(NativeAction::Invoke),
            ordinal_hint: Some(0),
        };
        assert!(matches(&first, &locator, &NativeAction::Invoke));
        assert!(!matches(&second, &locator, &NativeAction::Invoke));
        assert!(!matches(&first, &locator, &NativeAction::SetValue));
        let mut disabled = first.clone();
        disabled.node.enabled = false;
        assert!(!matches(&disabled, &locator, &NativeAction::Invoke));
    }

    #[test]
    fn public_names_redact_secrets_and_bound_length() {
        for value in [
            "person@example.org",
            "Bearer abc",
            "apikey_test",
            "Password",
            "123456789",
        ] {
            assert_eq!(redact(value), "redacted control");
        }
        assert_eq!(redact(&"中".repeat(100)).chars().count(), 80);
        assert_ne!(hash("value one"), hash("value two"));
        assert_eq!(hash("row context"), hash("row context"));
    }

    #[test]
    fn native_roles_and_predicates_match_shared_verifier_contract() {
        assert_eq!(role("AXTextArea"), UiRole::TextField);
        assert_eq!(role("AXRow"), UiRole::ListItem);
        assert_eq!(role("AXUnknown"), UiRole::Other);
        for (action, effect) in [
            (NativeAction::Invoke, "invoke_target_changed_or_disappeared"),
            (NativeAction::Toggle, "target_toggle_changed"),
            (NativeAction::Select, "target_selected"),
            (NativeAction::Focus, "target_focused"),
            (NativeAction::SetValue, "target_value_changed"),
        ] {
            assert!(kind(&action).is_ok());
            let predicates = effects(&action, "target");
            assert_eq!(predicates[0].name, effect);
            assert_eq!(predicates[0].arguments.get("target").unwrap(), "target");
        }
        assert!(kind(&NativeAction::Scroll).is_err());
        assert_eq!(
            effects(&NativeAction::Scroll, "target")[0].name,
            "target_scrolled"
        );
    }

    #[test]
    fn local_risk_does_not_require_confirmation_for_ordinary_workflow_steps() {
        assert_eq!(
            risk(&NativeAction::Invoke, Some("Open")),
            RiskClass::Reversible
        );
        assert_eq!(
            risk(&NativeAction::Invoke, Some("发送")),
            RiskClass::ExternalCommit
        );
        assert_eq!(
            risk(&NativeAction::Invoke, Some("永久删除")),
            RiskClass::DestructiveCritical
        );
        assert_eq!(risk(&NativeAction::SetValue, None), RiskClass::BoundedWrite);
    }

    #[test]
    fn same_named_windows_do_not_share_ephemeral_action_authority() {
        assert!(check_window_binding(17, 17).is_ok());
        assert!(check_window_binding(17, 18).is_err());
        assert!(check_window_binding(0, 0).is_err());
        assert!(window_is_unique(true, true, 1));
        assert!(!window_is_unique(true, true, 2));
        assert!(!window_is_unique(false, true, 1));
        assert!(!window_is_unique(true, false, 1));
    }

    #[test]
    fn timed_out_observation_is_an_error_and_traversal_keeps_node_and_depth_bounds() {
        use std::time::{Duration, Instant};
        assert!(check_deadline(Instant::now() - Duration::from_millis(1)).is_err());
        assert!(check_deadline(Instant::now() + Duration::from_secs(60)).is_ok());
        assert_eq!(child_capacity(200, 20, 30, 5), 149);
        assert_eq!(child_capacity(200, 199, 0, 5), 0);
        assert_eq!(child_capacity(200, 0, 0, 24), 0);
        assert_eq!(child_capacity(200, 0, 0, 23), 199);
    }

    #[test]
    fn closed_menus_do_not_expand_unless_the_branch_is_explicitly_requested() {
        assert!(!descend_menu("AXMenuBarItem", false, None, None, false));
        assert!(!descend_menu(
            "AXMenuItem",
            false,
            Some(false),
            Some(false),
            false
        ));
        assert!(descend_menu("AXMenuBar", false, None, None, false));
        assert!(descend_menu("AXMenu", false, None, None, false));
        assert!(descend_menu("AXMenuBarItem", true, None, None, false));
        assert!(descend_menu(
            "AXMenuBarItem",
            false,
            None,
            Some(true),
            false
        ));
        assert!(descend_menu("AXMenuItem", false, Some(true), None, false));
    }

    #[test]
    fn all_unique_observed_actions_remain_available_beyond_the_previous_37_limit() {
        let metadata: Vec<_> = (0..80)
            .map(|index| metadata(&format!("row-{index}")))
            .collect();
        let ranked = ranked_actions("Open", &metadata);
        assert_eq!(ranked.len(), 80);
        let mut ambiguous = metadata.clone();
        ambiguous.push(metadata[0].clone());
        assert_eq!(ranked_actions("Open", &ambiguous).len(), 79);
    }

    #[test]
    fn candidate_description_preserves_menu_origin_and_redacted_parent_context() {
        let mut item = metadata("preferences");
        item.origin = "menu".into();
        item.ancestors = vec![
            SemanticContext {
                role: Some(UiRole::Menu),
                automation_id: Some(MENU_SCOPE_ID.into()),
                accessible_name: Some("menu bar".into()),
            },
            SemanticContext {
                role: Some(UiRole::MenuItem),
                automation_id: None,
                accessible_name: Some("Apple".into()),
            },
        ];
        assert!(scope_is_menu(&item.ancestors));
        let label = describe(&item, &NativeAction::Invoke);
        assert!(label.contains("menu / menu bar > Apple"));
        item.ancestors[1].accessible_name = Some("person@example.org".into());
        assert!(!describe(&item, &NativeAction::Invoke).contains("person@example.org"));
        assert!(!scope_is_menu(&metadata("window-row").ancestors));
    }

    #[test]
    fn replay_restores_deep_region_using_stable_ancestors_and_fresh_runtime_ids() {
        let mut outer = metadata("unused");
        outer.node.opaque_id = "fresh-outer".into();
        outer.node.role = UiRole::Other;
        outer.identifier = Some("outer-panel".into());
        outer.raw_name = Some("Preferences".into());
        outer.ancestors.clear();
        let outer_context = SemanticContext {
            role: Some(outer.node.role.clone()),
            automation_id: outer.identifier.clone(),
            accessible_name: outer.raw_name.clone(),
        };
        let mut inner = outer.clone();
        inner.node.opaque_id = "fresh-inner".into();
        inner.identifier = Some("notification-settings".into());
        inner.raw_name = Some("Notifications".into());
        inner.ancestors = vec![outer_context.clone()];
        let inner_context = SemanticContext {
            role: Some(inner.node.role.clone()),
            automation_id: inner.identifier.clone(),
            accessible_name: inner.raw_name.clone(),
        };
        let mut target = metadata("unused");
        target.node.opaque_id = "fresh-target".into();
        target.ancestors = vec![outer_context, inner_context];
        let locator = SemanticLocator {
            app_id: "app.test".into(),
            window_id: None,
            window_title: None,
            role: Some(target.node.role.clone()),
            automation_id: target.identifier.clone(),
            accessible_name: target.raw_name.clone(),
            ancestor_chain: target.ancestors.clone(),
            supported_action: Some(NativeAction::Invoke),
            ordinal_hint: None,
        };
        assert_eq!(
            replay_resolution(std::slice::from_ref(&outer), &locator).unwrap(),
            ReplayResolution::Region("fresh-outer".into())
        );
        assert_eq!(
            replay_resolution(&[outer, inner.clone()], &locator).unwrap(),
            ReplayResolution::Region("fresh-inner".into())
        );
        assert_eq!(
            replay_resolution(&[inner, target], &locator).unwrap(),
            ReplayResolution::Target
        );
        assert!(!serde_json::to_string(&locator).unwrap().contains("fresh-"));
    }

    #[test]
    fn replay_rejects_ambiguous_regions_and_does_not_ignore_parent_context() {
        let mut target = metadata("intended-row");
        let locator = SemanticLocator {
            app_id: "app.test".into(),
            window_id: None,
            window_title: None,
            role: Some(target.node.role.clone()),
            automation_id: target.identifier.clone(),
            accessible_name: target.raw_name.clone(),
            ancestor_chain: target.ancestors.clone(),
            supported_action: Some(NativeAction::Invoke),
            ordinal_hint: None,
        };
        assert!(replay_resolution(&[target.clone(), target.clone()], &locator).is_err());
        target.ancestors[0].automation_id = Some("different-row".into());
        assert!(replay_resolution(&[target], &locator).is_err());
        assert!(replay_resolution(&[], &locator).is_err());
    }

    #[test]
    fn desired_checkbox_state_is_idempotent_and_unknown_is_not_false() {
        assert!(!checked_needs_press(Some(true), true).unwrap());
        assert!(!checked_needs_press(Some(false), false).unwrap());
        assert!(checked_needs_press(Some(false), true).unwrap());
        assert!(checked_needs_press(Some(true), false).unwrap());
        assert!(checked_needs_press(None, true).is_err());
        assert!(checked_needs_press(None, false).is_err());
        assert!(kind(&NativeAction::SetChecked).is_err());
        for checked in [false, true] {
            assert_eq!(
                kind_with_input(
                    &NativeAction::SetChecked,
                    &ExecutionInput {
                        checked: Some(checked),
                        ..Default::default()
                    }
                )
                .unwrap(),
                CandidateKind::SetChecked { checked }
            );
        }
    }

    #[test]
    fn scroll_candidates_require_advertised_direction_and_preserve_parameters() {
        let names = vec!["AXScrollDownByPage".into(), "AXScrollToVisible".into()];
        let supported = actions(
            &UiRole::List,
            false,
            &names,
            false,
            false,
            false,
            false,
            None,
        );
        assert!(supported.contains(&NativeAction::Scroll));
        assert!(supported.contains(&NativeAction::ScrollIntoView));
        let mut meta = metadata("list");
        meta.scroll_directions = scroll_directions(&names);
        let inputs = parameter_inputs(&meta, &NativeAction::Scroll);
        assert_eq!(inputs.len(), 1);
        assert_eq!(
            kind_with_input(&NativeAction::Scroll, &inputs[0]).unwrap(),
            CandidateKind::Scroll {
                direction: ScrollDirection::Down,
                amount: ScrollAmount::Page
            }
        );
        assert!(kind_with_input(
            &NativeAction::Scroll,
            &ExecutionInput {
                direction: Some(ScrollDirection::Down),
                amount: Some(ScrollAmount::Small),
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(
            parameter_inputs(&meta, &NativeAction::SetChecked)
                .iter()
                .filter_map(|input| input.checked)
                .collect::<Vec<_>>(),
            vec![true, false]
        );
    }

    #[test]
    fn range_values_are_finite_and_bounded_without_silent_clamping() {
        assert_eq!(range_value("0.5", 0.0, 1.0).unwrap(), 0.5);
        for invalid in ["NaN", "inf", "-inf", "2", "-1", "hello"] {
            assert!(range_value(invalid, 0.0, 1.0).is_err());
        }
        assert!(range_value("0", 1.0, 0.0).is_err());
    }

    #[test]
    fn chromium_legacy_ax_probe_is_only_used_for_unsupported_attribute() {
        assert!(legacy_ax_enablement_allowed(-25205));
        for other in [0, -25200, -25202, -25204, -25208] {
            assert!(!legacy_ax_enablement_allowed(other));
        }
    }

    #[test]
    fn hidden_elements_only_offer_their_advertised_scroll_into_view() {
        let mut meta = metadata("hidden");
        meta.node.visible = false;
        meta.node.supported_actions = vec![NativeAction::Invoke, NativeAction::ScrollIntoView];
        let ranked = ranked_actions("Open", &[meta]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].2, NativeAction::ScrollIntoView);
    }

    #[test]
    fn opening_security_preferences_is_navigation_not_permission_grant() {
        for name in ["Security settings", "安全设置", "打开系统设置"] {
            assert_eq!(
                risk(&NativeAction::Invoke, Some(name)),
                RiskClass::Reversible
            );
        }
        assert_eq!(
            risk(&NativeAction::Invoke, Some("允许访问")),
            RiskClass::DestructiveCritical
        );
    }

    #[test]
    fn readback_trust_follows_the_control_not_the_process_name() {
        let mut meta = metadata("input");
        assert_eq!(
            metadata_effects(&meta, &NativeAction::SetValue)[0].arguments["readback_trust"],
            "native"
        );
        meta.web_content = true;
        assert_eq!(
            metadata_effects(&meta, &NativeAction::SetValue)[0].arguments["readback_trust"],
            "web_content"
        );
    }

    #[test]
    fn bound_semantic_actions_can_run_without_the_foreground_window() {
        assert!(!requires_foreground(
            DeliveryMode::Auto,
            true,
            &NativeAction::Invoke
        ));
        assert!(!requires_foreground(
            DeliveryMode::Background,
            true,
            &NativeAction::SetValue
        ));
        assert!(requires_foreground(
            DeliveryMode::Foreground,
            true,
            &NativeAction::Invoke
        ));
        assert!(requires_foreground(
            DeliveryMode::Auto,
            false,
            &NativeAction::Invoke
        ));
        assert!(requires_foreground(
            DeliveryMode::Background,
            true,
            &NativeAction::Focus
        ));
    }

    #[test]
    fn native_child_pages_cover_large_sibling_lists_without_full_array_fetches() {
        let mut offset = 0;
        loop {
            let (count, more) = native_child_page(403, offset, 32);
            assert!(count <= 32);
            offset += count;
            if !more {
                break;
            }
        }
        assert_eq!(offset, 403);
        assert_eq!(native_child_page(403, 400, 200), (3, false));
        assert_eq!(native_child_page(403, 0, usize::MAX), (200, true));
        assert_eq!(native_child_page(403, 404, 32), (0, false));
        assert_eq!(native_child_page(403, 0, 0), (0, true));
    }

    #[test]
    fn accessibility_enablement_does_not_survive_a_reused_process_id() {
        assert!(same_process_lifetime(Some(1), Some(1)));
        assert!(!same_process_lifetime(Some(1), Some(2)));
        assert!(!same_process_lifetime(Some(1), None));
        assert!(!same_process_lifetime(None, None));
    }
}
