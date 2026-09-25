//! Windows UI Automation adapter for bounded semantic desktop actions.
//!
//! Native window handles, process identifiers, COM elements and geometry stay
//! inside this module. Callers receive only redacted semantic observations and
//! may execute only candidate ids created from the latest observation.

use super::runner::{CandidateBuilder, ComputerExecutor, ComputerObserver};
use super::types::*;

const DEFAULT_MAX_ELEMENTS: usize = 200;

#[cfg(windows)]
mod platform {
    use super::*;
    use async_trait::async_trait;
    use std::collections::{HashMap, VecDeque};
    use std::hash::{Hash, Hasher};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use uuid::Uuid;
    use windows::core::{Interface, BSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, BOOL, HWND};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
        IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
        IUIAutomationRangeValuePattern, IUIAutomationScrollItemPattern, IUIAutomationScrollPattern,
        IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationTreeWalker,
        IUIAutomationValuePattern, UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId,
        UIA_DataItemControlTypeId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
        UIA_ExpandCollapsePatternId, UIA_HyperlinkControlTypeId, UIA_InvokePatternId,
        UIA_ListControlTypeId, UIA_ListItemControlTypeId, UIA_MenuBarControlTypeId,
        UIA_MenuControlTypeId, UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId,
        UIA_RangeValuePatternId, UIA_ScrollItemPatternId, UIA_ScrollPatternId,
        UIA_SelectionItemPatternId, UIA_TabControlTypeId, UIA_TabItemControlTypeId,
        UIA_TextControlTypeId, UIA_TogglePatternId, UIA_TreeControlTypeId,
        UIA_TreeItemControlTypeId, UIA_ValuePatternId, UIA_WindowControlTypeId, UIA_CONTROLTYPE_ID,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, IsWindow, IsWindowVisible, SetForegroundWindow,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct NodeMetadata {
        opaque_id: String,
        role: UiRole,
        name: Option<String>,
        automation_id: Option<String>,
        enabled: bool,
        visible: bool,
        focused: bool,
        secure: bool,
        value_fingerprint: Option<u64>,
        toggled: Option<bool>,
        selected: Option<bool>,
        expanded: Option<bool>,
        ancestor_chain: Vec<SemanticContext>,
        supported_actions: Vec<NativeAction>,
        horizontal_scroll: bool,
        vertical_scroll: bool,
    }

    impl NodeMetadata {
        fn public_node(&self) -> UiNode {
            UiNode {
                opaque_id: self.opaque_id.clone(),
                semantic_key: self
                    .opaque_id
                    .strip_prefix("uie:")
                    .and_then(|suffix| suffix.rsplit_once(':'))
                    .map(|(key, _)| format!("uia:{key}")),
                role: self.role.clone(),
                // Password controls may expose provider-specific labels or
                // values through Name. Keep the raw locator local, but never
                // publish it to either decision provider.
                name: if self.secure { None } else { self.name.clone() },
                // UI values can contain private document content. The first
                // UIA slice intentionally exposes names, not arbitrary values.
                short_value: None,
                enabled: self.enabled,
                visible: self.visible,
                focused: self.focused,
                secure: self.secure,
                toggled: self.toggled,
                selected: self.selected,
                expanded: self.expanded,
                value_fingerprint: self
                    .value_fingerprint
                    .map(|value| format!("value:{value:016x}")),
                supported_actions: self.supported_actions.clone(),
            }
        }
    }

    #[derive(Debug, Clone)]
    struct InternalLocator {
        app_id: String,
        window_id: String,
        window_title: String,
        target_opaque_id: String,
        role: UiRole,
        automation_id: Option<String>,
        accessible_name: Option<String>,
        action: NativeAction,
        kind: CandidateKind,
        persistent: bool,
        risk: RiskClass,
        ancestor_chain: Vec<SemanticContext>,
        scope: ObservationScope,
        scope_root: Option<NodeMetadata>,
    }

    struct NativeNode {
        metadata: NodeMetadata,
        element: IUIAutomationElement,
    }

    struct NativeSnapshot {
        app: AppIdentity,
        window: WindowIdentity,
        nodes: Vec<NativeNode>,
        fingerprint: String,
        truncated: bool,
        stable_window_id: String,
        scope: ObservationScope,
        scope_root: Option<NodeMetadata>,
        // Keep COM initialized until every UIAutomation interface above has
        // been released. This field must remain last so it drops last.
        _com: ComApartment,
    }

    #[derive(Default)]
    struct AdapterState {
        last_fingerprint: Option<String>,
        revision: u64,
        metadata_by_node: HashMap<String, NodeMetadata>,
        locators_by_candidate: HashMap<String, InternalLocator>,
        stable_window_id: Option<String>,
        scope: ObservationScope,
        scope_root: Option<NodeMetadata>,
    }

    /// UIA-first adapter for a locally bound window, with foreground compatibility.
    pub struct WindowsUiaAdapter {
        max_elements: usize,
        state: Mutex<AdapterState>,
    }

    impl Default for WindowsUiaAdapter {
        fn default() -> Self {
            Self::new(DEFAULT_MAX_ELEMENTS)
        }
    }

    impl WindowsUiaAdapter {
        pub fn new(max_elements: usize) -> Self {
            Self {
                max_elements: max_elements.clamp(1, DEFAULT_MAX_ELEMENTS),
                state: Mutex::new(AdapterState::default()),
            }
        }

        fn capture_observation(
            &self,
            scope: &ObservationScope,
        ) -> Result<Observation, AutomationError> {
            let root = {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| AutomationError::Observation("UIA state unavailable".into()))?;
                match scope.subtree_id.as_deref() {
                    Some("@window" | "@menu") | None => None,
                    Some(id) => Some(
                        state
                            .metadata_by_node
                            .get(id)
                            .cloned()
                            .or_else(|| {
                                state
                                    .scope_root
                                    .as_ref()
                                    .filter(|root| root.opaque_id == id)
                                    .cloned()
                            })
                            .ok_or_else(|| {
                                AutomationError::Observation(
                                    "Unknown subtree_id; use an observed node id".into(),
                                )
                            })?,
                    ),
                }
            };
            let snapshot = capture_native(self.max_elements, scope, root.as_ref())?;
            validate_scope(scope, &snapshot)?;
            self.observation_from_snapshot(snapshot)
        }

        fn observation_from_snapshot(
            &self,
            snapshot: NativeSnapshot,
        ) -> Result<Observation, AutomationError> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Observation("UIA state lock was poisoned".into()))?;
            if state.last_fingerprint.as_deref() != Some(snapshot.fingerprint.as_str()) {
                state.revision = state.revision.saturating_add(1).max(1);
                state.last_fingerprint = Some(snapshot.fingerprint.clone());
            }
            state.stable_window_id = Some(snapshot.stable_window_id.clone());
            state.scope = snapshot.scope.clone();
            state.scope_root = snapshot.scope_root.clone();
            state.metadata_by_node = snapshot
                .nodes
                .iter()
                .map(|node| (node.metadata.opaque_id.clone(), node.metadata.clone()))
                .collect();

            Ok(Observation {
                revision: state.revision,
                fingerprint: snapshot.fingerprint,
                app: snapshot.app,
                window: snapshot.window,
                nodes: snapshot
                    .nodes
                    .into_iter()
                    .map(|node| node.metadata.public_node())
                    .collect(),
                captured_at_ms: now_ms(),
                truncated: snapshot.truncated,
            })
        }

        fn locator_for(
            &self,
            observation: &Observation,
            node: &UiNode,
            action: NativeAction,
            kind: CandidateKind,
        ) -> Result<Option<InternalLocator>, AutomationError> {
            let state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Candidates("UIA state lock was poisoned".into()))?;
            let metadata = state.metadata_by_node.get(&node.opaque_id).ok_or_else(|| {
                AutomationError::Candidates("observation metadata is no longer available".into())
            })?;
            let locator = InternalLocator {
                app_id: observation.app.id.clone(),
                window_id: observation.window.id.clone(),
                window_title: observation.window.title.clone(),
                target_opaque_id: node.opaque_id.clone(),
                role: metadata.role.clone(),
                automation_id: metadata.automation_id.clone(),
                accessible_name: metadata.name.clone(),
                risk: classify_risk(&action, metadata.name.as_deref()),
                action,
                kind,
                persistent: false,
                ancestor_chain: metadata.ancestor_chain.clone(),
                scope: state.scope.clone(),
                scope_root: state.scope_root.clone(),
            };
            // Do not offer a candidate that cannot be uniquely rebound from
            // stable semantics. A list reorder must never turn an ordinal into
            // permission to operate on another row.
            let matches = state
                .metadata_by_node
                .values()
                .filter(|current| internal_locator_matches(current, &locator))
                .count();
            Ok((matches == 1).then_some(locator))
        }

        fn execute_candidate(
            &self,
            _fresh: &Observation,
            action: &ActionCandidate,
            input: &ExecutionInput,
        ) -> Result<ActionReceipt, AutomationError> {
            let mut locator = self
                .state
                .lock()
                .map_err(|_| AutomationError::Execution("UIA state lock was poisoned".into()))?
                .locators_by_candidate
                .get(&action.id)
                .cloned()
                .ok_or_else(|| {
                    AutomationError::Execution(
                        "candidate was not created by this UIA adapter".into(),
                    )
                })?;
            if action.target.as_deref() != Some(locator.target_opaque_id.as_str())
                || action.kind != locator.kind
                || action.local_risk != locator.risk
            {
                return Err(AutomationError::Execution(
                    "candidate fields do not match the locally constructed UIA action".into(),
                ));
            }

            // Re-read the bound window tree immediately before dispatch. No COM
            // element or HWND from a previous observation is ever reused.
            locator.scope.delivery = input.delivery;
            if input.delivery == DeliveryMode::Background && locator.action == NativeAction::Focus {
                return Err(AutomationError::Execution(
                    "Focus requires foreground delivery".into(),
                ));
            }
            let mut snapshot = capture_native(
                self.max_elements,
                &locator.scope,
                locator.scope_root.as_ref(),
            )?;
            if snapshot.app.id != locator.app_id || snapshot.window.id != locator.window_id {
                return Err(AutomationError::Execution(
                    "bound UI changed before native dispatch".into(),
                ));
            }
            let hwnd = snapshot
                .scope
                .window_handle
                .map(|value| HWND(value as isize))
                .ok_or_else(|| {
                    AutomationError::Execution("UIA window binding is missing".into())
                })?;
            let initial_element = resolve_unique(&snapshot.nodes, &locator)?;
            let prepared_input = input_for_candidate(&locator, input)?;
            if let CandidateKind::SetChecked { checked } = action.kind {
                if unsafe {
                    initial_element
                        .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                        .and_then(|pattern| pattern.CurrentToggleState())
                }
                .ok()
                .and_then(toggle_boolean)
                    == Some(checked)
                {
                    return Ok(ActionReceipt {
                        candidate_id: action.id.clone(),
                        dispatched: false,
                        delivery_mode: None,
                        detail: Some("Target already has the requested checked state".into()),
                    });
                }
            }
            // WPF value/toggle providers focus their owning window.
            // Treat that provider requirement explicitly instead of promising
            // background input just because the transport is UIA.
            let framework =
                unsafe { resolve_unique(&snapshot.nodes, &locator)?.CurrentFrameworkId() }
                    .map(|value| value.to_string())
                    .unwrap_or_default();
            let provider_foreground = provider_requires_foreground(&framework, &locator.action);
            if provider_foreground && input.delivery == DeliveryMode::Background {
                return Err(AutomationError::Execution(crate::semantic::DesktopActionError::refused(
                    "background_unavailable: this WPF provider requires foreground delivery; no action sent")));
            }
            let needs_foreground = requires_foreground(snapshot.scope.delivery, &locator.action)
                || provider_foreground;
            if needs_foreground && unsafe { GetForegroundWindow() } != hwnd {
                let _ = unsafe { SetForegroundWindow(hwnd) };
                // Activation may open a modal or rebuild controls. Re-resolve
                // rather than dispatching an element read before activation.
                snapshot = capture_native(
                    self.max_elements,
                    &locator.scope,
                    locator.scope_root.as_ref(),
                )?;
                if snapshot.app.id != locator.app_id || snapshot.window.id != locator.window_id {
                    return Err(AutomationError::Execution(
                        "bound UI changed during activation".into(),
                    ));
                }
            }
            if needs_foreground && unsafe { GetForegroundWindow() } != hwnd {
                return Err(AutomationError::Execution(
                    "this UIA action requires foreground delivery; target activation failed".into(),
                ));
            }
            if !unsafe { IsWindow(hwnd) }.as_bool()
                || live_window_id(&snapshot.stable_window_id, hwnd) != snapshot.window.id
            {
                return Err(AutomationError::Execution(
                    "bound UIA window expired before dispatch".into(),
                ));
            }
            let element = resolve_unique(&snapshot.nodes, &locator)?;
            let input = prepared_input;
            let foreground_before = unsafe { GetForegroundWindow() };
            dispatch(element, &locator.action, &input)?;
            let provider_activated =
                foreground_before != hwnd && unsafe { GetForegroundWindow() } == hwnd;
            Ok(ActionReceipt {
                candidate_id: action.id.clone(),
                dispatched: true,
                delivery_mode: Some(if needs_foreground || provider_activated {
                    DeliveryMode::Foreground
                } else {
                    DeliveryMode::Background
                }),
                detail: Some(format!(
                    "Windows UIA {:?} dispatched{}",
                    locator.action,
                    if provider_activated {
                        "; native provider activated the target"
                    } else {
                        ""
                    }
                )),
            })
        }
    }

    #[async_trait]
    impl ComputerObserver for WindowsUiaAdapter {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities {
                supported: vec![
                    PlatformCapability::ReadSemanticTree,
                    PlatformCapability::NativeAction,
                    PlatformCapability::WriteValue,
                ],
                accessibility_permission: true,
            }
        }

        async fn observe(&self, scope: &ObservationScope) -> Result<Observation, AutomationError> {
            self.capture_observation(scope)
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
                subtree_id: None,
                tree_offset: 0,
                ..requested.clone()
            };
            if scope.window_handle.is_none() {
                if let Ok(state) = self.state.lock() {
                    if state.scope.app_id.as_deref() == Some(locator.app_id.as_str()) {
                        scope.window_handle = state.scope.window_handle;
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut visited = std::collections::HashSet::new();
            for _ in 0..64 {
                if Instant::now() >= deadline {
                    return Err(AutomationError::Observation(
                        "Saved UIA region resolution timed out".into(),
                    ));
                }
                let observation = self.capture_observation(&scope)?;
                let next = {
                    let state = self.state.lock().map_err(|_| {
                        AutomationError::Observation("UIA state unavailable".into())
                    })?;
                    replay_region(
                        &state.metadata_by_node.values().collect::<Vec<_>>(),
                        locator,
                    )
                };
                match next {
                    Ok(None) => {
                        if let Ok(state) = self.state.lock() {
                            scope.window_handle = state.scope.window_handle;
                        }
                        return Ok((observation, scope));
                    }
                    Ok(Some(id)) if visited.insert(id.clone()) => {
                        scope.subtree_id = Some(id);
                        scope.tree_offset = 0;
                    }
                    Err(error) if error.to_string().contains("ambiguous") => return Err(error),
                    _ if observation.truncated && observation.nodes.len() == self.max_elements => {
                        scope.tree_offset =
                            page_traversal_limit(scope.tree_offset, observation.nodes.len())?;
                    }
                    _ => return Err(AutomationError::Observation(
                        "Saved UIA target is missing from its ancestor region; refresh the locator"
                            .into(),
                    )),
                }
            }
            Err(AutomationError::Observation(
                "Saved UIA region exceeds bounded ancestor depth".into(),
            ))
        }
    }

    fn replay_region(
        nodes: &[&NodeMetadata],
        locator: &SemanticLocator,
    ) -> Result<Option<String>, AutomationError> {
        let target_count = nodes
            .iter()
            .filter(|node| {
                locator.role.as_ref().is_none_or(|v| v == &node.role)
                    && locator
                        .automation_id
                        .as_ref()
                        .is_none_or(|v| Some(v) == node.automation_id.as_ref())
                    && locator
                        .accessible_name
                        .as_ref()
                        .is_none_or(|v| Some(v) == node.name.as_ref())
                    && (locator.ancestor_chain.is_empty()
                        || locator.ancestor_chain == node.ancestor_chain)
            })
            .count();
        if target_count == 1 {
            return Ok(None);
        }
        if target_count > 1 {
            return Err(AutomationError::Observation(
                "Saved UIA target is ambiguous".into(),
            ));
        }
        for (index, ancestor) in locator.ancestor_chain.iter().enumerate().rev() {
            if ancestor.automation_id.is_none() && ancestor.accessible_name.is_none() {
                continue;
            }
            let matches: Vec<_> = nodes
                .iter()
                .filter(|node| {
                    ancestor.role.as_ref().is_none_or(|v| v == &node.role)
                        && ancestor
                            .automation_id
                            .as_ref()
                            .is_none_or(|v| Some(v) == node.automation_id.as_ref())
                        && ancestor
                            .accessible_name
                            .as_ref()
                            .is_none_or(|v| Some(v) == node.name.as_ref())
                        && node.ancestor_chain == locator.ancestor_chain[..index]
                })
                .collect();
            match matches.as_slice() {
                [node] => return Ok(Some(node.opaque_id.clone())),
                [] => {}
                _ => {
                    return Err(AutomationError::Observation(
                        "Saved UIA ancestor region is ambiguous".into(),
                    ))
                }
            }
        }
        Err(AutomationError::Observation(
            "Saved UIA target and its ancestor region are unavailable in the bounded observation"
                .into(),
        ))
    }

    impl CandidateBuilder for WindowsUiaAdapter {
        fn node_locator(
            &self,
            observation: &Observation,
            node: &UiNode,
        ) -> Option<SemanticLocator> {
            let state = self.state.lock().ok()?;
            let meta = state.metadata_by_node.get(&node.opaque_id)?;
            Some(SemanticLocator {
                app_id: observation.app.id.clone(),
                window_id: state
                    .stable_window_id
                    .clone()
                    .or_else(|| Some(observation.window.id.clone())),
                window_title: Some(observation.window.title.clone()),
                role: Some(meta.role.clone()),
                automation_id: meta.automation_id.clone(),
                accessible_name: meta.name.clone(),
                ancestor_chain: meta.ancestor_chain.clone(),
                supported_action: None,
                ordinal_hint: None,
            })
        }

        fn matches_window(&self, locator: &SemanticLocator, observation: &Observation) -> bool {
            locator.app_id == observation.app.id
                && locator
                    .window_id
                    .as_ref()
                    .map(|id| {
                        id == &observation.window.id
                            || self
                                .state
                                .lock()
                                .ok()
                                .is_some_and(|state| state.stable_window_id.as_ref() == Some(id))
                    })
                    .unwrap_or_else(|| {
                        locator
                            .window_title
                            .as_ref()
                            .is_none_or(|title| title == &observation.window.title)
                    })
        }

        fn rebuild_semantic_candidate_with_input(
            &self,
            locator: &SemanticLocator,
            action: NativeAction,
            observation: &Observation,
            input: &ExecutionInput,
        ) -> Result<ActionCandidate, AutomationError> {
            let mut candidate =
                self.rebuild_semantic_candidate(locator, action.clone(), observation)?;
            candidate.kind = match action {
                NativeAction::SetChecked => CandidateKind::SetChecked {
                    checked: input.checked.ok_or_else(|| {
                        AutomationError::Candidates("SetChecked requires checked".into())
                    })?,
                },
                NativeAction::Scroll => CandidateKind::Scroll {
                    direction: input.direction.ok_or_else(|| {
                        AutomationError::Candidates("Scroll requires direction".into())
                    })?,
                    amount: input.amount.unwrap_or(ScrollAmount::Small),
                },
                _ => candidate.kind,
            };
            let mut state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Candidates("UIA state unavailable".into()))?;
            if let Some(bound) = state.locators_by_candidate.get_mut(&candidate.id) {
                bound.kind = candidate.kind.clone();
            }
            Ok(candidate)
        }

        fn build(
            &self,
            goal: &str,
            observation: &Observation,
        ) -> Result<Vec<ActionCandidate>, AutomationError> {
            let mut ranked = Vec::new();

            for node in &observation.nodes {
                if !node.enabled {
                    continue;
                }
                for native_action in &node.supported_actions {
                    if !node.visible && *native_action != NativeAction::ScrollIntoView {
                        continue;
                    }
                    let kinds = {
                        let state = self.state.lock().map_err(|_| {
                            AutomationError::Candidates("UIA state unavailable".into())
                        })?;
                        let Some(meta) = state.metadata_by_node.get(&node.opaque_id) else {
                            continue;
                        };
                        candidate_kinds(native_action, meta)
                    };
                    for kind in kinds {
                        let Some(locator) = self.locator_for(
                            observation,
                            node,
                            native_action.clone(),
                            kind.clone(),
                        )?
                        else {
                            continue;
                        };
                        let id = format!("uia:{}", Uuid::new_v4().simple());
                        let candidate = ActionCandidate {
                            id: id.clone(),
                            observation_revision: observation.revision,
                            target: Some(node.opaque_id.clone()),
                            public_description: format!(
                                "{} [{}]",
                                describe_action_with_context(
                                    native_action,
                                    node,
                                    &locator.ancestor_chain,
                                ),
                                parameter_description(&kind)
                            ),
                            kind,
                            local_risk: classify_risk(native_action, node.name.as_deref()),
                            preconditions: vec![predicate(
                                "semantic_element_exists",
                                [("target", node.opaque_id.as_str())],
                            )],
                            expected_effects: expected_effects(native_action, node),
                        };
                        ranked.push((
                            candidate_relevance(goal, node, native_action)
                                + parameter_relevance(goal, &candidate.kind),
                            candidate,
                            locator,
                        ));
                    }
                }
            }
            ranked.sort_by_key(|item| std::cmp::Reverse(item.0));
            let mut candidate_locators = HashMap::new();
            let mut candidates = Vec::with_capacity(ranked.len() + 3);
            for (_, candidate, locator) in ranked {
                candidate_locators.insert(candidate.id.clone(), locator);
                candidates.push(candidate);
            }

            for (suffix, kind, description) in [
                ("done", CandidateKind::Done, "The bounded goal is complete"),
                (
                    "ask-user",
                    CandidateKind::AskUser,
                    "Ask the user for information needed to continue",
                ),
                (
                    "cannot-proceed",
                    CandidateKind::CannotProceed,
                    "No offered semantic action can advance the goal",
                ),
            ] {
                candidates.push(ActionCandidate {
                    id: format!("uia:{suffix}:{}", Uuid::new_v4().simple()),
                    observation_revision: observation.revision,
                    target: None,
                    kind,
                    public_description: description.into(),
                    local_risk: RiskClass::ReadOnly,
                    preconditions: vec![],
                    expected_effects: vec![],
                });
            }

            self.state
                .lock()
                .map_err(|_| AutomationError::Candidates("UIA state lock was poisoned".into()))?
                .locators_by_candidate = candidate_locators;
            Ok(candidates)
        }

        fn semantic_locator(&self, candidate: &ActionCandidate) -> Option<SemanticLocator> {
            let state = self.state.lock().ok()?;
            let locator = state.locators_by_candidate.get(&candidate.id)?;
            Some(SemanticLocator {
                app_id: locator.app_id.clone(),
                window_id: state
                    .stable_window_id
                    .clone()
                    .or_else(|| Some(locator.window_id.clone())),
                window_title: Some(locator.window_title.clone()),
                role: Some(locator.role.clone()),
                automation_id: locator.automation_id.clone(),
                accessible_name: locator.accessible_name.clone(),
                ancestor_chain: locator.ancestor_chain.clone(),
                supported_action: Some(locator.action.clone()),
                ordinal_hint: None,
            })
        }

        fn rebuild_semantic_candidate(
            &self,
            locator: &SemanticLocator,
            action: NativeAction,
            observation: &Observation,
        ) -> Result<ActionCandidate, AutomationError> {
            if locator.app_id != observation.app.id {
                return Err(AutomationError::Candidates(
                    "foreground application does not match the saved semantic locator".into(),
                ));
            }
            let stable_window_id = self
                .state
                .lock()
                .ok()
                .and_then(|state| state.stable_window_id.clone());
            let wrong_window = locator
                .window_id
                .as_ref()
                .map(|id| id != &observation.window.id && Some(id) != stable_window_id.as_ref())
                .unwrap_or_else(|| {
                    locator
                        .window_title
                        .as_ref()
                        .is_some_and(|title| title != &observation.window.title)
                });
            if wrong_window {
                return Err(AutomationError::Candidates(
                    "foreground window does not match the saved semantic locator".into(),
                ));
            }
            if locator
                .supported_action
                .as_ref()
                .is_some_and(|saved| saved != &action)
            {
                return Err(AutomationError::Candidates(
                    "saved semantic action does not match the locator capability".into(),
                ));
            }
            if !matches!(
                action,
                NativeAction::Invoke
                    | NativeAction::Toggle
                    | NativeAction::Select
                    | NativeAction::Expand
                    | NativeAction::Collapse
                    | NativeAction::Focus
                    | NativeAction::SetValue
                    | NativeAction::SetChecked
                    | NativeAction::Scroll
                    | NativeAction::ScrollIntoView
                    | NativeAction::SetRangeValue
            ) {
                return Err(AutomationError::Candidates(format!(
                    "Windows UIA persistent action {action:?} is unsupported"
                )));
            }

            let state = self
                .state
                .lock()
                .map_err(|_| AutomationError::Candidates("UIA state lock was poisoned".into()))?;
            let matches: Vec<_> = observation
                .nodes
                .iter()
                .filter_map(|node| {
                    let metadata = state.metadata_by_node.get(&node.opaque_id)?;
                    let matches = locator
                        .role
                        .as_ref()
                        .is_none_or(|role| role == &metadata.role)
                        && locator
                            .automation_id
                            .as_ref()
                            .is_none_or(|id| metadata.automation_id.as_ref() == Some(id))
                        && locator
                            .accessible_name
                            .as_ref()
                            .is_none_or(|name| metadata.name.as_ref() == Some(name))
                        && (locator.ancestor_chain.is_empty()
                            || locator.ancestor_chain == metadata.ancestor_chain)
                        && metadata.supported_actions.contains(&action);
                    matches.then_some((node, metadata))
                })
                .collect();
            let (node, metadata) = match matches.as_slice() {
                [unique] => *unique,
                [] => {
                    return Err(AutomationError::Candidates(
                        "saved semantic locator did not resolve to an actionable UIA element"
                            .into(),
                    ))
                }
                _ => {
                    return Err(AutomationError::Candidates(
                        "saved semantic locator is ambiguous; add a stable automation id or ancestor/row context"
                            .into(),
                    ))
                }
            };
            let internal = InternalLocator {
                app_id: observation.app.id.clone(),
                window_id: observation.window.id.clone(),
                window_title: observation.window.title.clone(),
                target_opaque_id: node.opaque_id.clone(),
                role: metadata.role.clone(),
                automation_id: metadata.automation_id.clone(),
                accessible_name: metadata.name.clone(),
                action: action.clone(),
                kind: candidate_kind(&action),
                persistent: true,
                risk: classify_risk(&action, metadata.name.as_deref()),
                ancestor_chain: metadata.ancestor_chain.clone(),
                scope: state.scope.clone(),
                scope_root: state.scope_root.clone(),
            };
            let id = format!("uia:persisted:{}", Uuid::new_v4().simple());
            let candidate = ActionCandidate {
                id: id.clone(),
                observation_revision: observation.revision,
                target: Some(node.opaque_id.clone()),
                kind: candidate_kind(&action),
                public_description: describe_action_with_context(
                    &action,
                    node,
                    &internal.ancestor_chain,
                ),
                local_risk: internal.risk,
                preconditions: vec![predicate(
                    "semantic_element_exists",
                    [("target", node.opaque_id.as_str())],
                )],
                expected_effects: expected_effects(&action, node),
            };
            drop(state);
            self.state
                .lock()
                .map_err(|_| AutomationError::Candidates("UIA state lock was poisoned".into()))?
                .locators_by_candidate
                .insert(id, internal);
            Ok(candidate)
        }
    }

    #[async_trait]
    impl ComputerExecutor for WindowsUiaAdapter {
        async fn execute(
            &self,
            fresh: &Observation,
            action: &ActionCandidate,
            input: &ExecutionInput,
        ) -> Result<ActionReceipt, AutomationError> {
            self.execute_candidate(fresh, action, input)
        }
    }

    fn requires_foreground(delivery: DeliveryMode, action: &NativeAction) -> bool {
        delivery == DeliveryMode::Foreground || *action == NativeAction::Focus
    }

    fn provider_requires_foreground(framework: &str, action: &NativeAction) -> bool {
        // Standard WPF peers activate their window for these patterns (covered
        // by the opt-in real WPF fixture). ScrollItem remains target-addressed.
        framework.eq_ignore_ascii_case("WPF")
            && matches!(
                action,
                NativeAction::SetValue
                    | NativeAction::Toggle
                    | NativeAction::SetChecked
                    | NativeAction::SetRangeValue
                    | NativeAction::Scroll
                    | NativeAction::Invoke
            )
    }

    fn page_traversal_limit(offset: usize, page_size: usize) -> Result<usize, AutomationError> {
        offset
            .checked_add(page_size)
            .filter(|limit| *limit <= 10_000)
            .ok_or_else(|| {
                AutomationError::Observation(
                    "UIA tree offset exceeds the bounded search budget; select a smaller subtree"
                        .into(),
                )
            })
    }

    fn resolve_window(scope: &ObservationScope) -> Result<HWND, AutomationError> {
        if let Some(handle) = scope.window_handle {
            let window = HWND(handle as isize);
            return if unsafe { IsWindow(window) }.as_bool() {
                Ok(window)
            } else {
                Err(AutomationError::Observation(
                    "Bound UIA window no longer exists".into(),
                ))
            };
        }
        let foreground = unsafe { GetForegroundWindow() };
        let Some(app_id) = scope.app_id.as_deref() else {
            return Ok(foreground);
        };
        unsafe extern "system" fn collect(
            hwnd: HWND,
            data: windows::Win32::Foundation::LPARAM,
        ) -> BOOL {
            if IsWindowVisible(hwnd).as_bool() {
                // EnumWindows invokes this callback synchronously with the live vector.
                (*(data.0 as *mut Vec<HWND>)).push(hwnd);
            }
            BOOL(1)
        }
        let mut windows = Vec::<HWND>::new();
        unsafe {
            EnumWindows(
                Some(collect),
                windows::Win32::Foundation::LPARAM((&mut windows as *mut Vec<HWND>) as isize),
            )
        }
        .map_err(|error| uia_observation_error("discover bound windows", error))?;
        let matching: Vec<_> = windows
            .into_iter()
            .filter(|hwnd| {
                process_image_path(*hwnd).is_some_and(|path| {
                    let seed = app_identity_seed(None, &window_class(*hwnd), &path.to_lowercase());
                    format!("windows-app:{:016x}", stable_hash(&seed)) == app_id
                })
            })
            .collect();
        match matching.as_slice() {
            [window] => Ok(*window),
            _ if matching.contains(&foreground) && scope.delivery == DeliveryMode::Foreground => {
                Ok(foreground)
            }
            [] => Err(AutomationError::Observation(
                "Requested UIA application has no available window".into(),
            )),
            _ => Err(AutomationError::Observation(
                "Requested UIA application has multiple windows; bind a specific window".into(),
            )),
        }
    }

    fn capture_native(
        max_elements: usize,
        scope: &ObservationScope,
        subtree: Option<&NodeMetadata>,
    ) -> Result<NativeSnapshot, AutomationError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let com = ComApartment::initialize()?;
        let hwnd = resolve_window(scope)?;
        if hwnd.0 == 0 {
            return Err(AutomationError::Observation(
                "Windows has no available target window".into(),
            ));
        }
        let title = window_title(hwnd);
        let window_class = window_class(hwnd);
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| uia_observation_error("create UI Automation client", error))?
        };
        // The traversal deadline alone cannot interrupt a blocked provider call.
        // UIAutomation8 supports per-connection and per-transaction timeouts.
        let timeouts = automation
            .cast::<IUIAutomation2>()
            .map_err(|error| uia_observation_error("configure UI Automation timeout", error))?;
        unsafe {
            timeouts
                .SetConnectionTimeout(1000)
                .and_then(|_| timeouts.SetTransactionTimeout(1000))
                .map_err(|error| uia_observation_error("set UI Automation timeout", error))?;
        }
        let root = unsafe {
            automation
                .ElementFromHandle(hwnd)
                .map_err(|error| uia_observation_error("read foreground window", error))?
        };
        let framework = unsafe { root.CurrentFrameworkId() }
            .ok()
            .map(|value| truncate(&value.to_string(), 64))
            .filter(|value| !value.is_empty());
        let root_name = element_text(unsafe { root.CurrentName() }.ok(), 128);
        let display_name = root_name
            .clone()
            .or_else(|| nonempty(title.clone()))
            .or_else(|| nonempty(window_class.clone()))
            .unwrap_or_else(|| "Windows application".into());
        let process_identity = process_image_path(hwnd).unwrap_or_default().to_lowercase();
        let app_seed = app_identity_seed(framework.as_deref(), &window_class, &process_identity);
        let app = AppIdentity {
            id: format!("windows-app:{:016x}", stable_hash(&app_seed)),
            display_name,
        };
        let window_title = nonempty(title)
            .or(root_name)
            .unwrap_or_else(|| "Untitled".into());
        let root_automation_id = element_text(unsafe { root.CurrentAutomationId() }.ok(), 256);
        let stable_window_id = format!(
            "windows-window:{:016x}",
            stable_hash(&format!(
                "{}|{}|{}",
                app.id,
                window_class,
                root_automation_id.as_deref().unwrap_or_default()
            ))
        );
        let window = WindowIdentity {
            id: live_window_id(&stable_window_id, hwnd),
            title: window_title,
        };

        let walker = unsafe {
            automation
                .ControlViewWalker()
                .map_err(|error| uia_observation_error("create UIA control-view walker", error))?
        };
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        let focused_menu = focused_menu_root(&automation, &walker, pid, deadline);
        // Walk lazily instead of FindAll, which materializes the entire provider tree.
        let scoped_root = if scope.subtree_id.as_deref().is_none_or(|id| id == "@window") {
            root.clone()
        } else if scope.subtree_id.as_deref() == Some("@menu") && focused_menu.is_some() {
            focused_menu.clone().expect("checked focused menu")
        } else {
            let mut pending = VecDeque::from([(root.clone(), 0usize)]);
            if let Some(menu) = focused_menu {
                pending.push_back((menu, 0));
            }
            let mut selected = None;
            let mut inspected = 0;
            let mut search_truncated = false;
            let mut seen: Vec<IUIAutomationElement> = Vec::new();
            while let Some((element, depth)) = pending.pop_front() {
                if Instant::now() >= deadline || inspected >= 2000 {
                    search_truncated = true;
                    break;
                }
                if seen.iter().any(|old| {
                    unsafe { automation.CompareElements(old, &element) }
                        .is_ok_and(|same| same.as_bool())
                }) {
                    continue;
                }
                seen.push(element.clone());
                inspected += 1;
                let ancestors = semantic_ancestor_chain(&automation, &walker, &element, &root);
                if let Ok(node) = native_node(element.clone(), inspected, ancestors) {
                    let matched = if let Some(subtree) = subtree {
                        same_container(&node.metadata, subtree)
                    } else {
                        node.metadata.role == UiRole::Menu && node.metadata.visible
                    };
                    if matched {
                        if selected.is_some() {
                            return Err(AutomationError::Observation("Requested UIA subtree is ambiguous; observe a distinct named container".into()));
                        }
                        selected = Some(element.clone());
                        if subtree.is_none() {
                            break;
                        }
                    }
                }
                search_truncated |= enqueue_children(
                    &walker,
                    &element,
                    depth,
                    &mut pending,
                    2000 - inspected,
                    deadline,
                );
            }
            if subtree.is_some() && search_truncated {
                return Err(AutomationError::Observation(
                    "Could not uniquely locate the UIA subtree within the observation budget"
                        .into(),
                ));
            }
            selected.ok_or_else(|| AutomationError::Observation("Requested UIA subtree is unavailable within the bounded tree; observe @window again".into()))?
        };
        let mut nodes = Vec::with_capacity(max_elements);
        let mut pending = VecDeque::from([(scoped_root, 0usize)]);
        let traversal_limit = page_traversal_limit(scope.tree_offset, max_elements)?;
        let mut visited = 0usize;
        let mut truncated = scope.tree_offset > 0;
        let mut captured_root = None;
        while let Some((element, depth)) = pending.pop_front() {
            if nodes.len() >= max_elements || Instant::now() >= deadline {
                truncated = true;
                break;
            }
            let ancestors = semantic_ancestor_chain(&automation, &walker, &element, &root);
            let index = visited;
            visited += 1;
            // A stale provider must not shift the continuation offset by silently
            // dropping a node; refresh the observation instead.
            let node = native_node(element.clone(), index, ancestors)?;
            if captured_root.is_none() {
                captured_root = Some(node.metadata.clone());
            }
            let is_menu = node.metadata.role == UiRole::Menu;
            if index >= scope.tree_offset {
                nodes.push(node);
            }
            // The window-content pass retains the menu entry itself but does
            // not spend its body budget traversing the full menu hierarchy.
            // Explicit @menu or region observations expand that hierarchy.
            if scope.subtree_id.as_deref().is_none_or(|id| id == "@window") && is_menu {
                continue;
            }
            truncated |= enqueue_children(
                &walker,
                &element,
                depth,
                &mut pending,
                traversal_limit.saturating_sub(visited),
                deadline,
            );
        }
        if Instant::now() >= deadline {
            return Err(AutomationError::Observation(
                "UIA observation exceeded its time budget".into(),
            ));
        }
        let scope_root = scope
            .subtree_id
            .as_ref()
            .filter(|id| id.as_str() != "@window")
            .and_then(|id| {
                captured_root.map(|mut root| {
                    // Keep the original region alias across a fresh traversal whose
                    // local index starts at zero (and across later native pages).
                    root.opaque_id = id.clone();
                    root
                })
            });

        let mut bound_scope = scope.clone();
        bound_scope.window_handle = Some(hwnd.0 as i32);
        bound_scope.app_id = Some(app.id.clone());

        let fingerprint = fingerprint(&app, &window, &nodes);
        Ok(NativeSnapshot {
            app,
            window,
            nodes,
            fingerprint,
            truncated,
            stable_window_id,
            scope: bound_scope,
            scope_root,
            _com: com,
        })
    }

    fn live_window_id(stable: &str, hwnd: HWND) -> String {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        window_runtime_key(stable, hwnd.0, pid)
    }

    fn focused_menu_root(
        automation: &IUIAutomation,
        walker: &IUIAutomationTreeWalker,
        pid: u32,
        deadline: Instant,
    ) -> Option<IUIAutomationElement> {
        let mut element = unsafe { automation.GetFocusedElement() }.ok()?;
        if unsafe { element.CurrentProcessId() }.ok()? != pid as i32 {
            return None;
        }
        let mut menu = None;
        for _ in 0..24 {
            if Instant::now() >= deadline {
                break;
            }
            if unsafe { element.CurrentControlType() }
                .ok()
                .is_some_and(|role| {
                    role == UIA_MenuControlTypeId || role == UIA_MenuBarControlTypeId
                })
            {
                menu = Some(element.clone());
            }
            let Ok(parent) = (unsafe { walker.GetParentElement(&element) }) else {
                break;
            };
            if unsafe { parent.CurrentProcessId() }.ok() != Some(pid as i32) {
                break;
            }
            element = parent;
        }
        menu
    }

    fn window_runtime_key(stable: &str, hwnd: isize, pid: u32) -> String {
        format!(
            "windows-live:{:016x}",
            stable_hash(&format!("{stable}|{hwnd}|{pid}"))
        )
    }

    fn same_container(left: &NodeMetadata, right: &NodeMetadata) -> bool {
        left.role == right.role
            && left.automation_id == right.automation_id
            && left.name == right.name
            && left.ancestor_chain == right.ancestor_chain
    }

    fn enqueue_children(
        walker: &IUIAutomationTreeWalker,
        element: &IUIAutomationElement,
        depth: usize,
        pending: &mut VecDeque<(IUIAutomationElement, usize)>,
        capacity: usize,
        deadline: Instant,
    ) -> bool {
        let mut child = unsafe { walker.GetFirstChildElement(element) }.ok();
        if depth >= 24 {
            return child.is_some();
        }
        while let Some(current) = child {
            if pending.len() >= capacity || Instant::now() >= deadline {
                return true;
            }
            child = unsafe { walker.GetNextSiblingElement(&current) }.ok();
            pending.push_back((current, depth + 1));
        }
        false
    }

    fn native_node(
        element: IUIAutomationElement,
        index: usize,
        ancestor_chain: Vec<SemanticContext>,
    ) -> Result<NativeNode, AutomationError> {
        let control_type = unsafe { element.CurrentControlType() }
            .map_err(|error| uia_observation_error("read UIA control type", error))?;
        let name = element_text(unsafe { element.CurrentName() }.ok(), 256);
        let automation_id = element_text(unsafe { element.CurrentAutomationId() }.ok(), 256);
        let enabled = bool_or(unsafe { element.CurrentIsEnabled() }, false);
        let visible = !bool_or(unsafe { element.CurrentIsOffscreen() }, true);
        let focused = bool_or(unsafe { element.CurrentHasKeyboardFocus() }, false);
        let secure = bool_or(unsafe { element.CurrentIsPassword() }, false);
        let mut supported_actions = Vec::new();
        if supports_pattern::<IUIAutomationInvokePattern>(&element, UIA_InvokePatternId) {
            supported_actions.push(NativeAction::Invoke);
        }
        let toggle_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        }
        .ok();
        if toggle_pattern.is_some() {
            supported_actions.push(NativeAction::Toggle);
            supported_actions.push(NativeAction::SetChecked);
        }
        let scroll_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        }
        .ok();
        let horizontal_scroll = scroll_pattern.as_ref().is_some_and(|pattern| {
            bool_or(unsafe { pattern.CurrentHorizontallyScrollable() }, false)
        });
        let vertical_scroll = scroll_pattern.as_ref().is_some_and(|pattern| {
            bool_or(unsafe { pattern.CurrentVerticallyScrollable() }, false)
        });
        if horizontal_scroll || vertical_scroll {
            supported_actions.push(NativeAction::Scroll);
        }
        if supports_pattern::<IUIAutomationScrollItemPattern>(&element, UIA_ScrollItemPatternId) {
            supported_actions.push(NativeAction::ScrollIntoView);
        }
        let range_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
        }
        .ok();
        if range_pattern
            .as_ref()
            .is_some_and(|pattern| !bool_or(unsafe { pattern.CurrentIsReadOnly() }, true))
            && !secure
        {
            supported_actions.push(NativeAction::SetRangeValue);
        }
        let selection_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                UIA_SelectionItemPatternId,
            )
        }
        .ok();
        let selected = selection_pattern
            .as_ref()
            .and_then(|pattern| unsafe { pattern.CurrentIsSelected() }.ok())
            .map(|value| value.as_bool());
        if selection_pattern.is_some() {
            supported_actions.push(NativeAction::Select);
        }
        let expand_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                UIA_ExpandCollapsePatternId,
            )
        }
        .ok();
        let expanded = expand_pattern
            .as_ref()
            .and_then(|pattern| unsafe { pattern.CurrentExpandCollapseState() }.ok())
            .and_then(|state| {
                if state == windows::Win32::UI::Accessibility::ExpandCollapseState_Expanded {
                    Some(true)
                } else if state == windows::Win32::UI::Accessibility::ExpandCollapseState_Collapsed
                {
                    Some(false)
                } else {
                    None
                }
            });
        if expand_pattern.is_some() {
            match expanded {
                Some(true) => supported_actions.push(NativeAction::Collapse),
                Some(false) => supported_actions.push(NativeAction::Expand),
                None => {
                    supported_actions.push(NativeAction::Expand);
                    supported_actions.push(NativeAction::Collapse);
                }
            }
        }
        let value_pattern =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
                .ok();
        let writable_value = value_pattern
            .as_ref()
            .and_then(|pattern| unsafe { pattern.CurrentIsReadOnly() }.ok())
            .is_some_and(|value| !value.as_bool());
        if writable_value && !secure {
            supported_actions.push(NativeAction::SetValue);
        }
        let value_fingerprint = if secure {
            None
        } else {
            value_pattern
                .as_ref()
                .and_then(|pattern| unsafe { pattern.CurrentValue() }.ok())
                .map(|value| stable_hash(&value.to_string()))
                .or_else(|| {
                    range_pattern
                        .as_ref()
                        .and_then(|pattern| unsafe { pattern.CurrentValue() }.ok())
                        .map(|value| stable_hash(&value.to_string()))
                })
        };
        let toggled = toggle_pattern
            .as_ref()
            .and_then(|pattern| unsafe { pattern.CurrentToggleState() }.ok())
            .and_then(toggle_boolean);
        if bool_or(unsafe { element.CurrentIsKeyboardFocusable() }, false) {
            supported_actions.push(NativeAction::Focus);
        }
        let role = role_for(control_type);
        // Keep a stable semantic prefix across list reordering while retaining
        // the local observation index as a collision suffix. The prefix is an
        // opaque hash only; no ancestor text is exposed to decision providers.
        let semantic_hash = stable_hash(&format!(
            "{role:?}|{}|{}|{ancestor_chain:?}",
            name.as_deref().unwrap_or_default(),
            automation_id.as_deref().unwrap_or_default()
        ));
        let opaque_id = format!("uie:{semantic_hash:016x}:{index}");
        Ok(NativeNode {
            metadata: NodeMetadata {
                opaque_id,
                role,
                name,
                automation_id,
                enabled,
                visible,
                focused,
                secure,
                value_fingerprint,
                toggled,
                selected,
                expanded,
                ancestor_chain,
                supported_actions,
                horizontal_scroll,
                vertical_scroll,
            },
            element,
        })
    }

    fn supports_pattern<T: Interface>(
        element: &IUIAutomationElement,
        pattern: windows::Win32::UI::Accessibility::UIA_PATTERN_ID,
    ) -> bool {
        unsafe { element.GetCurrentPatternAs::<T>(pattern) }.is_ok()
    }

    fn semantic_ancestor_chain(
        automation: &IUIAutomation,
        walker: &IUIAutomationTreeWalker,
        element: &IUIAutomationElement,
        root: &IUIAutomationElement,
    ) -> Vec<SemanticContext> {
        const MAX_ANCESTORS: usize = 8;
        let mut chain = Vec::new();
        let mut current = element.clone();
        for _ in 0..MAX_ANCESTORS {
            let Ok(parent) = (unsafe { walker.GetParentElement(&current) }) else {
                break;
            };
            let is_root = unsafe { automation.CompareElements(&parent, root) }
                .map(|same| same.as_bool())
                .unwrap_or(false);
            if is_root {
                break;
            }
            if let Some(context) = semantic_context(&parent) {
                chain.push(context);
            }
            current = parent;
        }
        chain.reverse();
        chain
    }

    fn semantic_context(element: &IUIAutomationElement) -> Option<SemanticContext> {
        let role = unsafe { element.CurrentControlType() }.ok().map(role_for);
        let automation_id = element_text(unsafe { element.CurrentAutomationId() }.ok(), 256);
        let accessible_name = element_text(unsafe { element.CurrentName() }.ok(), 256);
        let has_stable_text = automation_id.is_some() || accessible_name.is_some();
        let is_structural = role.as_ref().is_some_and(|role| {
            matches!(
                role,
                UiRole::List
                    | UiRole::ListItem
                    | UiRole::Menu
                    | UiRole::MenuItem
                    | UiRole::Tab
                    | UiRole::Document
            )
        });
        (has_stable_text || is_structural).then_some(SemanticContext {
            role,
            automation_id,
            accessible_name,
        })
    }

    fn resolve_unique<'a>(
        nodes: &'a [NativeNode],
        locator: &InternalLocator,
    ) -> Result<&'a IUIAutomationElement, AutomationError> {
        let matches: Vec<_> = nodes
            .iter()
            .filter(|node| {
                node.metadata.role == locator.role
                    && node.metadata.automation_id == locator.automation_id
                    && node.metadata.name == locator.accessible_name
                    && node.metadata.ancestor_chain == locator.ancestor_chain
                    && node.metadata.supported_actions.contains(&locator.action)
            })
            .collect();
        match matches.as_slice() {
            [unique] => Ok(&unique.element),
            [] => Err(AutomationError::Execution(
                "semantic UIA target is missing after fresh re-observation".into(),
            )),
            _ => Err(AutomationError::Execution(
                "semantic UIA target is ambiguous after fresh re-observation; refusing ordinal fallback"
                    .into(),
            )),
        }
    }

    fn dispatch(
        element: &IUIAutomationElement,
        action: &NativeAction,
        input: &ExecutionInput,
    ) -> Result<(), AutomationError> {
        let result = unsafe {
            match action {
                NativeAction::Invoke => element
                    .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    .and_then(|pattern| pattern.Invoke()),
                NativeAction::Toggle => element
                    .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    .and_then(|pattern| pattern.Toggle()),
                NativeAction::SetChecked => {
                    let checked = input.checked.ok_or_else(|| {
                        AutomationError::Execution(
                            "SetChecked requires an explicit checked value".into(),
                        )
                    })?;
                    let pattern = element
                        .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                        .map_err(|error| uia_observation_error("read toggle capability", error))?;
                    // Three-state controls may need two transitions. Read every state;
                    // mixed is unknown, never an alias for unchecked.
                    let mut seen = Vec::new();
                    for _ in 0..2 {
                        let current = pattern
                            .CurrentToggleState()
                            .map_err(|error| uia_observation_error("read toggle state", error))?;
                        if toggle_boolean(current) == Some(checked) {
                            return Ok(());
                        }
                        if seen.contains(&current) {
                            return Err(AutomationError::Execution(
                                "UIA checkbox state did not settle; reobserve before retrying"
                                    .into(),
                            ));
                        }
                        seen.push(current);
                        pattern.Toggle().map_err(|error| {
                            AutomationError::Execution(format!("UIA SetChecked failed: {error}"))
                        })?;
                    }
                    if pattern.CurrentToggleState().ok().and_then(toggle_boolean) == Some(checked) {
                        Ok(())
                    } else {
                        return Err(AutomationError::Execution("UIA checkbox did not reach the requested state; reobserve before retrying".into()));
                    }
                }
                NativeAction::Scroll => {
                    let direction = input.direction.ok_or_else(|| {
                        AutomationError::Execution("Scroll requires a direction".into())
                    })?;
                    let amount = input.amount.unwrap_or(ScrollAmount::Small);
                    let (horizontal, vertical) = native_scroll_amounts(direction, amount);
                    element
                        .GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                        .and_then(|pattern| pattern.Scroll(horizontal, vertical))
                }
                NativeAction::ScrollIntoView => element
                    .GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
                    .and_then(|pattern| pattern.ScrollIntoView()),
                NativeAction::SetRangeValue => {
                    let value = parse_range_value(input.value.as_deref())?;
                    let pattern = element
                        .GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(
                            UIA_RangeValuePatternId,
                        )
                        .map_err(|error| uia_observation_error("read range capability", error))?;
                    let minimum = pattern
                        .CurrentMinimum()
                        .map_err(|error| uia_observation_error("read range minimum", error))?;
                    let maximum = pattern
                        .CurrentMaximum()
                        .map_err(|error| uia_observation_error("read range maximum", error))?;
                    if value < minimum || value > maximum {
                        return Err(AutomationError::Execution(format!(
                            "Range value must be within {minimum}..={maximum}"
                        )));
                    }
                    pattern.SetValue(value)
                }
                NativeAction::Select => element
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                    .and_then(|pattern| pattern.Select()),
                NativeAction::Expand => element
                    .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                    .and_then(|pattern| pattern.Expand()),
                NativeAction::Collapse => element
                    .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                    .and_then(|pattern| pattern.Collapse()),
                NativeAction::Focus => element.SetFocus(),
                NativeAction::SetValue => {
                    let value = input.value.as_deref().ok_or_else(|| {
                        AutomationError::Execution(
                            "SetValue requires caller-provided text at dispatch time".into(),
                        )
                    })?;
                    let value = BSTR::from(value);
                    element
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .and_then(|pattern| pattern.SetValue(&value))
                }
                unsupported => {
                    return Err(AutomationError::Execution(format!(
                        "native UIA action {unsupported:?} is not supported by this slice"
                    )))
                }
            }
        };
        result.map_err(|error| AutomationError::Execution(format!("UIA dispatch failed: {error}")))
    }

    fn toggle_boolean(state: windows::Win32::UI::Accessibility::ToggleState) -> Option<bool> {
        use windows::Win32::UI::Accessibility::{ToggleState_Off, ToggleState_On};
        if state == ToggleState_On {
            Some(true)
        } else if state == ToggleState_Off {
            Some(false)
        } else {
            None
        }
    }

    fn parse_range_value(value: Option<&str>) -> Result<f64, AutomationError> {
        value
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .ok_or_else(|| {
                AutomationError::Execution("SetRangeValue requires a finite numeric value".into())
            })
    }

    fn native_scroll_amounts(
        direction: ScrollDirection,
        amount: ScrollAmount,
    ) -> (
        windows::Win32::UI::Accessibility::ScrollAmount,
        windows::Win32::UI::Accessibility::ScrollAmount,
    ) {
        use windows::Win32::UI::Accessibility::{
            ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement, ScrollAmount_NoAmount,
            ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement,
        };
        let increment = matches!(direction, ScrollDirection::Right | ScrollDirection::Down);
        let amount = match (increment, amount) {
            (true, ScrollAmount::Small) => ScrollAmount_SmallIncrement,
            (true, ScrollAmount::Page) => ScrollAmount_LargeIncrement,
            (false, ScrollAmount::Small) => ScrollAmount_SmallDecrement,
            (false, ScrollAmount::Page) => ScrollAmount_LargeDecrement,
        };
        if matches!(direction, ScrollDirection::Left | ScrollDirection::Right) {
            (amount, ScrollAmount_NoAmount)
        } else {
            (ScrollAmount_NoAmount, amount)
        }
    }

    fn candidate_kinds(action: &NativeAction, metadata: &NodeMetadata) -> Vec<CandidateKind> {
        match action {
            NativeAction::SetChecked => vec![
                CandidateKind::SetChecked { checked: true },
                CandidateKind::SetChecked { checked: false },
            ],
            NativeAction::Scroll => [
                ScrollDirection::Up,
                ScrollDirection::Down,
                ScrollDirection::Left,
                ScrollDirection::Right,
            ]
            .into_iter()
            .filter(|direction| {
                if matches!(direction, ScrollDirection::Up | ScrollDirection::Down) {
                    metadata.vertical_scroll
                } else {
                    metadata.horizontal_scroll
                }
            })
            .flat_map(|direction| {
                [ScrollAmount::Small, ScrollAmount::Page]
                    .into_iter()
                    .map(move |amount| CandidateKind::Scroll { direction, amount })
            })
            .collect(),
            _ => vec![candidate_kind(action)],
        }
    }

    fn parameter_description(kind: &CandidateKind) -> String {
        match kind {
            CandidateKind::SetChecked { checked } => format!("checked={checked}"),
            CandidateKind::Scroll { direction, amount } => format!("{direction:?} {amount:?}"),
            CandidateKind::SetRangeValue { .. } => "finite numeric value".into(),
            _ => "native".into(),
        }
    }

    fn input_for_candidate(
        locator: &InternalLocator,
        input: &ExecutionInput,
    ) -> Result<ExecutionInput, AutomationError> {
        let mut result = input.clone();
        match locator.kind {
            CandidateKind::SetChecked { checked } if !locator.persistent => {
                if input.checked.is_some_and(|value| value != checked) {
                    return Err(AutomationError::Execution(
                        "checked parameter does not match the selected candidate".into(),
                    ));
                }
                result.checked = Some(checked);
            }
            CandidateKind::Scroll { direction, amount } if !locator.persistent => {
                if input.direction.is_some_and(|value| value != direction)
                    || input.amount.is_some_and(|value| value != amount)
                {
                    return Err(AutomationError::Execution(
                        "scroll parameters do not match the selected candidate".into(),
                    ));
                }
                result.direction = Some(direction);
                result.amount = Some(amount);
            }
            _ => {}
        }
        Ok(result)
    }

    fn candidate_kind(action: &NativeAction) -> CandidateKind {
        match action {
            NativeAction::Invoke => CandidateKind::Invoke,
            NativeAction::Toggle => CandidateKind::Toggle,
            NativeAction::SetChecked => CandidateKind::SetChecked { checked: true },
            NativeAction::Scroll => CandidateKind::Scroll {
                direction: ScrollDirection::Down,
                amount: ScrollAmount::Small,
            },
            NativeAction::ScrollIntoView => CandidateKind::ScrollIntoView,
            NativeAction::SetRangeValue => CandidateKind::SetRangeValue {
                slot_id: "value".into(),
            },
            NativeAction::Select => CandidateKind::Select,
            NativeAction::Expand => CandidateKind::Expand,
            NativeAction::Collapse => CandidateKind::Collapse,
            NativeAction::Focus => CandidateKind::Focus,
            NativeAction::SetValue => CandidateKind::SetValue {
                slot_id: "value".into(),
            },
            _ => unreachable!("candidate builder filters to the supported UIA slice"),
        }
    }

    fn describe_action(action: &NativeAction, node: &UiNode) -> String {
        let name = node
            .name
            .as_deref()
            .map(redact_public_name)
            .unwrap_or_else(|| "unnamed control".into());
        format!("{action:?} {:?} '{name}'", node.role)
    }

    fn describe_action_with_context(
        action: &NativeAction,
        node: &UiNode,
        ancestors: &[SemanticContext],
    ) -> String {
        let contexts: Vec<_> = ancestors
            .iter()
            .rev()
            .take(3)
            .rev()
            .filter_map(|parent| {
                parent
                    .accessible_name
                    .as_deref()
                    .or(parent.automation_id.as_deref())
                    .map(redact_public_name)
            })
            .collect();
        let action = describe_action(action, node);
        if contexts.is_empty() {
            action
        } else {
            format!("{action} in {}", contexts.join(" > "))
        }
    }

    fn candidate_relevance(goal: &str, node: &UiNode, action: &NativeAction) -> i32 {
        let goal = goal.to_lowercase();
        let name = node.name.as_deref().unwrap_or_default().to_lowercase();
        let mut score = 0_i32;
        if !name.is_empty() {
            score += 20;
            if goal.contains(&name) {
                score += 120;
            }
            for token in goal.split(|ch: char| !ch.is_alphanumeric()) {
                if token.chars().count() >= 2 && name.contains(token) {
                    score += 35;
                }
            }
            // Chinese UI labels do not have whitespace-delimited words. Match
            // short character n-grams, bounded to avoid rewarding long labels.
            let chars: Vec<_> = goal
                .chars()
                .filter(|ch| ('\u{3400}'..='\u{9fff}').contains(ch))
                .take(128)
                .collect();
            score += chars
                .windows(2)
                .filter(|pair| name.contains(&pair.iter().collect::<String>()))
                .take(6)
                .count() as i32
                * 20;
            // "Settings" is not "Open System Settings" when no system scope
            // was requested. Extra label qualifiers should lower its rank.
            for qualifier in ["system", "permission", "系统", "权限"] {
                if name.contains(qualifier) && !goal.contains(qualifier) {
                    score -= 45;
                }
            }
        }
        if node.focused {
            score += 30;
        }
        score += match action {
            NativeAction::Invoke | NativeAction::SetValue | NativeAction::SetRangeValue => 18,
            NativeAction::SetChecked | NativeAction::Select => 16,
            NativeAction::Toggle => 8,
            NativeAction::Expand | NativeAction::Collapse => 10,
            NativeAction::Focus => 2,
            _ => 0,
        };
        score
    }

    fn parameter_relevance(goal: &str, kind: &CandidateKind) -> i32 {
        let goal = goal.to_lowercase();
        match kind {
            CandidateKind::SetChecked { checked } => {
                let disable = [
                    "uncheck",
                    "disable",
                    "turn off",
                    "取消勾选",
                    "取消选中",
                    "关闭",
                    "禁用",
                ]
                .iter()
                .any(|word| goal.contains(word));
                if *checked != disable {
                    15
                } else {
                    0
                }
            }
            CandidateKind::Scroll { direction, .. } => {
                let words = match direction {
                    ScrollDirection::Up => ["up", "上"],
                    ScrollDirection::Down => ["down", "下"],
                    ScrollDirection::Left => ["left", "左"],
                    ScrollDirection::Right => ["right", "右"],
                };
                if words.iter().any(|word| goal.contains(word)) {
                    15
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    fn redact_public_name(value: &str) -> String {
        let lower = value.to_lowercase();
        let has_long_digit_run = value
            .split(|ch: char| !ch.is_ascii_digit())
            .any(|part| part.len() >= 8);
        if value.contains('@')
            || has_long_digit_run
            || lower.contains("apikey_")
            || lower.contains("api_key")
            || lower.contains("bearer ")
            || lower.contains("password")
            || lower.contains("密码")
        {
            "redacted control".into()
        } else {
            truncate(value, 80)
        }
    }

    fn classify_risk(action: &NativeAction, name: Option<&str>) -> RiskClass {
        crate::semantic::classify_desktop_risk(action, name)
    }

    fn predicate<'a>(
        name: &str,
        arguments: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Predicate {
        Predicate {
            name: name.into(),
            arguments: arguments
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    fn expected_effects(action: &NativeAction, node: &UiNode) -> Vec<Predicate> {
        let effect = match action {
            NativeAction::Invoke => "invoke_target_changed_or_disappeared",
            NativeAction::Toggle => "target_toggle_changed",
            NativeAction::SetChecked => "target_checked",
            NativeAction::Scroll => "target_scroll_changed",
            NativeAction::ScrollIntoView => "target_visible",
            NativeAction::SetRangeValue => "target_value_changed",
            NativeAction::Select => "target_selected",
            NativeAction::Expand => "target_expanded",
            NativeAction::Collapse => "target_collapsed",
            NativeAction::Focus => "target_focused",
            NativeAction::SetValue => "target_value_changed",
            _ => return Vec::new(),
        };
        vec![predicate(effect, [("target", node.opaque_id.as_str())])]
    }

    fn internal_locator_matches(metadata: &NodeMetadata, locator: &InternalLocator) -> bool {
        metadata.enabled
            && (metadata.visible || locator.action == NativeAction::ScrollIntoView)
            && metadata.role == locator.role
            && metadata.automation_id == locator.automation_id
            && metadata.name == locator.accessible_name
            && metadata.ancestor_chain == locator.ancestor_chain
            && metadata.supported_actions.contains(&locator.action)
    }

    fn validate_scope(
        scope: &ObservationScope,
        snapshot: &NativeSnapshot,
    ) -> Result<(), AutomationError> {
        if scope
            .app_id
            .as_ref()
            .is_some_and(|expected| expected != &snapshot.app.id)
        {
            return Err(AutomationError::Observation(
                "foreground application is outside the requested scope".into(),
            ));
        }
        if scope.window_id.as_ref().is_some_and(|expected| {
            expected != &snapshot.window.id && expected != &snapshot.stable_window_id
        }) {
            return Err(AutomationError::Observation(
                "foreground window is outside the requested scope".into(),
            ));
        }
        Ok(())
    }

    fn fingerprint(app: &AppIdentity, window: &WindowIdentity, nodes: &[NativeNode]) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        app.id.hash(&mut hasher);
        window.title.hash(&mut hasher);
        for node in nodes {
            std::mem::discriminant(&node.metadata.role).hash(&mut hasher);
            node.metadata.name.hash(&mut hasher);
            node.metadata.automation_id.hash(&mut hasher);
            node.metadata.enabled.hash(&mut hasher);
            node.metadata.visible.hash(&mut hasher);
            node.metadata.focused.hash(&mut hasher);
            node.metadata.secure.hash(&mut hasher);
            node.metadata.value_fingerprint.hash(&mut hasher);
            node.metadata.toggled.hash(&mut hasher);
            node.metadata.selected.hash(&mut hasher);
            node.metadata.expanded.hash(&mut hasher);
            for action in &node.metadata.supported_actions {
                std::mem::discriminant(action).hash(&mut hasher);
            }
        }
        format!("uia:{:016x}", hasher.finish())
    }

    fn role_for(control_type: UIA_CONTROLTYPE_ID) -> UiRole {
        match control_type {
            value if value == UIA_WindowControlTypeId => UiRole::Window,
            value if value == UIA_ButtonControlTypeId => UiRole::Button,
            value if value == UIA_EditControlTypeId => UiRole::TextField,
            value if value == UIA_CheckBoxControlTypeId => UiRole::CheckBox,
            value if value == UIA_RadioButtonControlTypeId => UiRole::RadioButton,
            value if value == UIA_ListControlTypeId => UiRole::List,
            value if value == UIA_ListItemControlTypeId => UiRole::ListItem,
            value if value == UIA_MenuControlTypeId || value == UIA_MenuBarControlTypeId => {
                UiRole::Menu
            }
            value if value == UIA_MenuItemControlTypeId => UiRole::MenuItem,
            value if value == UIA_TabControlTypeId || value == UIA_TabItemControlTypeId => {
                UiRole::Tab
            }
            value if value == UIA_DocumentControlTypeId => UiRole::Document,
            // These controls remain actionable through their advertised UIA
            // patterns even though the shared role vocabulary is compact.
            value
                if value == UIA_TextControlTypeId
                    || value == UIA_HyperlinkControlTypeId
                    || value == UIA_TreeControlTypeId
                    || value == UIA_TreeItemControlTypeId
                    || value == UIA_DataItemControlTypeId =>
            {
                UiRole::Other
            }
            _ => UiRole::Other,
        }
    }

    fn bool_or(result: windows::core::Result<BOOL>, fallback: bool) -> bool {
        result.map(|value| value.as_bool()).unwrap_or(fallback)
    }

    fn element_text(value: Option<windows::core::BSTR>, max_chars: usize) -> Option<String> {
        value
            .map(|value| truncate(&value.to_string(), max_chars))
            .and_then(nonempty)
    }

    fn nonempty(value: String) -> Option<String> {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    fn truncate(value: &str, max_chars: usize) -> String {
        value.chars().take(max_chars).collect()
    }

    fn window_title(hwnd: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
        let mut buffer = vec![0_u16; length.saturating_add(1).max(1)];
        let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
        String::from_utf16_lossy(&buffer[..copied])
    }

    fn window_class(hwnd: HWND) -> String {
        let mut buffer = vec![0_u16; 256];
        let copied = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
        truncate(&String::from_utf16_lossy(&buffer[..copied]), 128)
    }

    /// The image path is consumed only as local hash input for AppIdentity.
    /// Neither the path nor PID is exposed in Observation or sent to Jev.
    fn process_image_path(hwnd: HWND) -> Option<String> {
        let mut pid = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            return None;
        }
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut buffer = vec![0_u16; 1024];
        let mut size = buffer.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        (!path.is_empty()).then_some(path)
    }

    fn stable_hash(value: &str) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    fn app_identity_seed(
        framework: Option<&str>,
        window_class: &str,
        process_identity: &str,
    ) -> String {
        if !process_identity.is_empty() {
            // A native application's main window and its owned dialogs often
            // use different framework/class values. The executable identity is
            // the stable application boundary across those transitions.
            format!("process|{process_identity}")
        } else {
            // Some protected/system windows do not expose their image path. In
            // that case retain the previous local-only fallback rather than
            // merging every unknown foreground application.
            format!(
                "fallback|{}|{}",
                framework.unwrap_or("win32"),
                window_class.to_lowercase()
            )
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }

    fn uia_observation_error(context: &str, error: windows::core::Error) -> AutomationError {
        AutomationError::Observation(format!("{context}: {error}"))
    }

    struct ComApartment(bool);

    impl ComApartment {
        fn initialize() -> Result<Self, AutomationError> {
            let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if result.is_ok() {
                Ok(Self(true))
            } else {
                // RPC_E_CHANGED_MODE means this thread already has a usable
                // COM apartment with another concurrency model.
                const RPC_E_CHANGED_MODE: i32 = unchecked_hresult(0x80010106);
                if result.0 == RPC_E_CHANGED_MODE {
                    Ok(Self(false))
                } else {
                    Err(AutomationError::Observation(format!(
                        "initialize COM for UIA: {result:?}"
                    )))
                }
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    const fn unchecked_hresult(value: u32) -> i32 {
        value as i32
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn context(name: &str) -> SemanticContext {
            SemanticContext {
                role: Some(UiRole::ListItem),
                automation_id: None,
                accessible_name: Some(name.into()),
            }
        }

        fn metadata(id: &str, ancestors: Vec<SemanticContext>) -> NodeMetadata {
            NodeMetadata {
                opaque_id: id.into(),
                role: UiRole::Button,
                name: Some("Open".into()),
                automation_id: Some("open-button".into()),
                enabled: true,
                visible: true,
                focused: false,
                secure: false,
                value_fingerprint: None,
                toggled: None,
                selected: None,
                expanded: None,
                ancestor_chain: ancestors,
                supported_actions: vec![NativeAction::Invoke],
                horizontal_scroll: false,
                vertical_scroll: false,
            }
        }

        #[test]
        fn replay_descends_stable_ancestry_without_reusing_old_node_ids() {
            let parent = context("Row A");
            let mut region = metadata("fresh-region", vec![]);
            region.role = UiRole::ListItem;
            region.name = Some("Row A".into());
            region.automation_id = None;
            let target = metadata("fresh-button", vec![parent.clone()]);
            let locator = SemanticLocator {
                app_id: "app".into(),
                window_id: None,
                window_title: None,
                role: Some(UiRole::Button),
                automation_id: Some("open-button".into()),
                accessible_name: Some("Open".into()),
                ancestor_chain: vec![parent],
                supported_action: Some(NativeAction::Invoke),
                ordinal_hint: None,
            };
            assert_eq!(
                replay_region(&[&region], &locator).unwrap(),
                Some("fresh-region".into())
            );
            assert_eq!(replay_region(&[&target], &locator).unwrap(), None);
            assert!(replay_region(&[&target, &target], &locator).is_err());
            region.ancestor_chain = vec![context("wrong document")];
            assert!(replay_region(&[&region], &locator).is_err());
        }

        fn observation(nodes: &[NodeMetadata]) -> Observation {
            Observation {
                revision: 1,
                fingerprint: "test".into(),
                app: AppIdentity {
                    id: "app".into(),
                    display_name: "App".into(),
                },
                window: WindowIdentity {
                    id: "window".into(),
                    title: "Window".into(),
                },
                nodes: nodes.iter().map(NodeMetadata::public_node).collect(),
                captured_at_ms: 1,
                truncated: false,
            }
        }

        fn adapter_with(nodes: &[NodeMetadata]) -> WindowsUiaAdapter {
            let adapter = WindowsUiaAdapter::default();
            adapter.state.lock().unwrap().metadata_by_node = nodes
                .iter()
                .cloned()
                .map(|node| (node.opaque_id.clone(), node))
                .collect();
            adapter
        }

        fn locator(ancestors: Vec<SemanticContext>, ordinal_hint: Option<u16>) -> SemanticLocator {
            SemanticLocator {
                app_id: "app".into(),
                window_id: Some("window".into()),
                window_title: Some("Window".into()),
                role: Some(UiRole::Button),
                automation_id: Some("open-button".into()),
                accessible_name: Some("Open".into()),
                ancestor_chain: ancestors,
                supported_action: Some(NativeAction::Invoke),
                ordinal_hint,
            }
        }

        #[test]
        fn legacy_ordinal_never_selects_an_ambiguous_duplicate() {
            let nodes = vec![metadata("first", vec![]), metadata("second", vec![])];
            let adapter = adapter_with(&nodes);
            let error = adapter
                .rebuild_semantic_candidate(
                    &locator(vec![], Some(1)),
                    NativeAction::Invoke,
                    &observation(&nodes),
                )
                .expect_err("ordinal fallback must not resolve duplicate controls");
            assert!(error.to_string().contains("ambiguous"));
        }

        #[test]
        fn ancestor_row_context_survives_list_reordering() {
            let row_a = metadata("row-a-open", vec![context("Row A")]);
            let row_b = metadata("row-b-open", vec![context("Row B")]);
            // The current tree is deliberately in the opposite order from the
            // saved row semantics. Resolution must follow context, not index.
            let reordered = vec![row_b.clone(), row_a.clone()];
            let adapter = adapter_with(&reordered);
            let candidate = adapter
                .rebuild_semantic_candidate(
                    &locator(vec![context("Row B")], Some(0)),
                    NativeAction::Invoke,
                    &observation(&reordered),
                )
                .expect("stable row context should resolve uniquely");
            assert_eq!(candidate.target.as_deref(), Some("row-b-open"));
        }

        #[test]
        fn ambiguous_candidates_are_not_exposed_in_action_space() {
            let nodes = vec![metadata("first", vec![]), metadata("second", vec![])];
            let adapter = adapter_with(&nodes);
            let candidates = adapter
                .build("open", &observation(&nodes))
                .expect("candidate construction should still return control choices");
            assert!(!candidates
                .iter()
                .any(|candidate| candidate.kind == CandidateKind::Invoke));
        }

        #[test]
        fn all_candidates_remain_available_for_pagination_and_retain_scope() {
            let nodes: Vec<_> = (0..60)
                .map(|index| {
                    metadata(
                        &format!("button-{index}"),
                        vec![context(&format!("Row {index}"))],
                    )
                })
                .collect();
            let adapter = adapter_with(&nodes);
            adapter.state.lock().unwrap().scope.subtree_id = Some("@menu".into());
            let candidates = adapter.build("Open", &observation(&nodes)).unwrap();
            assert_eq!(
                candidates
                    .iter()
                    .filter(|c| c.kind == CandidateKind::Invoke)
                    .count(),
                60
            );
            assert!(candidates
                .iter()
                .any(|c| c.public_description.contains("Row 59")));
            let state = adapter.state.lock().unwrap();
            assert!(state.locators_by_candidate.values().all(|locator| locator
                .scope
                .subtree_id
                .as_deref()
                == Some("@menu")));
        }

        #[test]
        fn same_class_windows_have_separate_live_identities_but_durable_locators() {
            assert_ne!(
                window_runtime_key("same-class", 1, 42),
                window_runtime_key("same-class", 2, 42)
            );
            assert_ne!(
                window_runtime_key("same-class", 1, 42),
                window_runtime_key("same-class", 1, 43)
            );
            let node = metadata("button", vec![]);
            let adapter = adapter_with(std::slice::from_ref(&node));
            adapter.state.lock().unwrap().stable_window_id = Some("stable-window".into());
            let candidates = adapter.build("Open", &observation(&[node])).unwrap();
            let saved = adapter.semantic_locator(&candidates[0]).unwrap();
            assert_eq!(saved.window_id.as_deref(), Some("stable-window"));
        }

        #[test]
        fn ancestor_descriptions_are_redacted() {
            let node = metadata("button", vec![]).public_node();
            let description = describe_action_with_context(
                &NativeAction::Invoke,
                &node,
                &[context("person@example.com")],
            );
            assert!(description.contains("redacted control"));
            assert!(!description.contains("person@example.com"));
        }

        #[test]
        fn critical_risk_covers_account_payment_and_destructive_system_actions() {
            for label in [
                "Delete account",
                "恢复出厂设置",
                "Format drive",
                "Confirm transfer",
                "提交订单",
                "Grant permission",
            ] {
                assert_eq!(
                    classify_risk(&NativeAction::Invoke, Some(label)),
                    RiskClass::DestructiveCritical,
                    "{label} must be gated as a major irreversible action"
                );
            }

            assert_eq!(
                classify_risk(&NativeAction::Invoke, Some("Delete draft")),
                RiskClass::Reversible,
                "ordinary reversible deletion must not be over-gated"
            );
        }

        #[test]
        fn settings_navigation_is_not_a_payment_or_permission_commit() {
            for label in [
                "Security Settings",
                "安全设置",
                "打开系统设置",
                "Payment history",
                "支付设置",
                "购买记录",
                "权限说明",
            ] {
                assert_eq!(
                    classify_risk(&NativeAction::Invoke, Some(label)),
                    RiskClass::Reversible,
                    "{label}"
                );
            }
            for label in [
                "Confirm purchase",
                "确认支付",
                "Grant permission",
                "恢复出厂设置",
                "Restore factory settings",
                "Delete all data settings",
                "永久删除历史记录",
                "删除账户设置",
            ] {
                assert_eq!(
                    classify_risk(&NativeAction::Invoke, Some(label)),
                    RiskClass::DestructiveCritical,
                    "{label}"
                );
            }
        }

        #[test]
        fn mixed_toggle_is_unknown_and_idempotent_choices_include_both_states() {
            use windows::Win32::UI::Accessibility::{
                ToggleState_Indeterminate, ToggleState_Off, ToggleState_On,
            };
            assert_eq!(toggle_boolean(ToggleState_Indeterminate), None);
            assert_eq!(toggle_boolean(ToggleState_On), Some(true));
            assert_eq!(toggle_boolean(ToggleState_Off), Some(false));
            let mut node = metadata("check", vec![]);
            node.supported_actions = vec![NativeAction::SetChecked];
            let adapter = adapter_with(std::slice::from_ref(&node));
            let candidates = adapter.build("取消勾选", &observation(&[node])).unwrap();
            assert!(candidates
                .iter()
                .any(|c| c.kind == CandidateKind::SetChecked { checked: true }));
            assert_eq!(
                candidates[0].kind,
                CandidateKind::SetChecked { checked: false }
            );
            let state = adapter.state.lock().unwrap();
            let bound = state.locators_by_candidate.get(&candidates[0].id).unwrap();
            assert_eq!(
                input_for_candidate(bound, &ExecutionInput::default())
                    .unwrap()
                    .checked,
                Some(false)
            );
            assert!(input_for_candidate(
                bound,
                &ExecutionInput {
                    checked: Some(true),
                    ..Default::default()
                }
            )
            .is_err());
        }

        #[test]
        fn scroll_candidates_follow_reported_axes_and_range_rejects_non_finite_values() {
            let mut node = metadata("list", vec![]);
            node.vertical_scroll = true;
            let vertical = candidate_kinds(&NativeAction::Scroll, &node);
            assert_eq!(vertical.len(), 4);
            assert!(vertical.iter().all(|kind| matches!(
                kind,
                CandidateKind::Scroll {
                    direction: ScrollDirection::Up | ScrollDirection::Down,
                    ..
                }
            )));
            node.horizontal_scroll = true;
            assert_eq!(candidate_kinds(&NativeAction::Scroll, &node).len(), 8);
            assert_eq!(parse_range_value(Some("3.5")).unwrap(), 3.5);
            for value in [None, Some("NaN"), Some("inf"), Some("five")] {
                assert!(parse_range_value(value).is_err());
            }
            let (horizontal, vertical) =
                native_scroll_amounts(ScrollDirection::Left, ScrollAmount::Page);
            assert_eq!(
                horizontal,
                windows::Win32::UI::Accessibility::ScrollAmount_LargeDecrement
            );
            assert_eq!(
                vertical,
                windows::Win32::UI::Accessibility::ScrollAmount_NoAmount
            );
        }

        #[test]
        fn background_native_actions_do_not_require_foreground_but_focus_does() {
            for action in [
                NativeAction::SetValue,
                NativeAction::SetChecked,
                NativeAction::SetRangeValue,
                NativeAction::Scroll,
                NativeAction::Invoke,
            ] {
                assert!(provider_requires_foreground("WPF", &action));
                assert!(!provider_requires_foreground("Win32", &action));
            }
            assert!(!provider_requires_foreground(
                "WPF",
                &NativeAction::ScrollIntoView
            ));
            for action in [
                NativeAction::Invoke,
                NativeAction::SetValue,
                NativeAction::SetChecked,
                NativeAction::Scroll,
            ] {
                assert!(!requires_foreground(DeliveryMode::Auto, &action));
                assert!(requires_foreground(DeliveryMode::Foreground, &action));
            }
            assert!(requires_foreground(
                DeliveryMode::Auto,
                &NativeAction::Focus
            ));
        }

        #[test]
        fn native_page_budget_includes_skipped_nodes_instead_of_repeating_first_page() {
            assert_eq!(page_traversal_limit(200, 200).unwrap(), 400);
            assert_eq!(page_traversal_limit(400, 200).unwrap(), 600);
            assert!(page_traversal_limit(usize::MAX, 200).is_err());
            assert!(page_traversal_limit(10_000, 200).is_err());
        }

        #[test]
        fn chinese_and_english_intents_rank_app_settings_above_system_settings() {
            for (goal, exact, system) in [
                ("打开应用中的设置", "设置", "打开系统设置"),
                ("open app settings", "Settings", "Open System Settings"),
            ] {
                let mut desired = metadata("desired", vec![]).public_node();
                desired.name = Some(exact.into());
                let mut other = desired.clone();
                other.name = Some(system.into());
                assert!(
                    candidate_relevance(goal, &desired, &NativeAction::Invoke)
                        > candidate_relevance(goal, &other, &NativeAction::Invoke)
                );
            }
        }

        #[test]
        fn application_identity_survives_owned_dialog_window_classes() {
            let executable = r"c:\windows\system32\notepad.exe";
            assert_eq!(
                app_identity_seed(Some("Win32"), "Notepad", executable),
                app_identity_seed(Some("Win32"), "#32770", executable)
            );
        }

        #[test]
        fn application_identity_fallback_keeps_unknown_window_classes_separate() {
            assert_ne!(
                app_identity_seed(Some("Win32"), "FirstWindow", ""),
                app_identity_seed(Some("Win32"), "SecondWindow", "")
            );
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    use async_trait::async_trait;

    /// Explicit non-Windows stub. macOS and Linux adapters will use their
    /// native accessibility APIs rather than pretending UIA is available.
    pub struct WindowsUiaAdapter {
        _max_elements: usize,
    }

    impl Default for WindowsUiaAdapter {
        fn default() -> Self {
            Self::new(DEFAULT_MAX_ELEMENTS)
        }
    }

    impl WindowsUiaAdapter {
        pub fn new(max_elements: usize) -> Self {
            Self {
                _max_elements: max_elements.clamp(1, DEFAULT_MAX_ELEMENTS),
            }
        }
    }

    #[async_trait]
    impl ComputerObserver for WindowsUiaAdapter {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities {
                supported: vec![],
                accessibility_permission: false,
            }
        }

        async fn observe(&self, _scope: &ObservationScope) -> Result<Observation, AutomationError> {
            Err(AutomationError::Observation(
                "Windows UI Automation is unsupported on this platform".into(),
            ))
        }
    }

    impl CandidateBuilder for WindowsUiaAdapter {
        fn build(
            &self,
            _goal: &str,
            _observation: &Observation,
        ) -> Result<Vec<ActionCandidate>, AutomationError> {
            Err(AutomationError::Candidates(
                "Windows UI Automation is unsupported on this platform".into(),
            ))
        }

        fn rebuild_semantic_candidate(
            &self,
            _locator: &SemanticLocator,
            _action: NativeAction,
            _observation: &Observation,
        ) -> Result<ActionCandidate, AutomationError> {
            Err(AutomationError::Candidates(
                "persistent Windows UI Automation actions are unsupported on this platform".into(),
            ))
        }
    }

    #[async_trait]
    impl ComputerExecutor for WindowsUiaAdapter {
        async fn execute(
            &self,
            _fresh: &Observation,
            _action: &ActionCandidate,
            _input: &ExecutionInput,
        ) -> Result<ActionReceipt, AutomationError> {
            Err(AutomationError::Execution(
                "Windows UI Automation is unsupported on this platform".into(),
            ))
        }
    }
}

pub use platform::WindowsUiaAdapter;

#[cfg(all(test, windows))]
mod windows_smoke_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, SetForegroundWindow,
    };

    struct WpfFixture {
        child: Child,
        artifacts: PathBuf,
    }

    impl WpfFixture {
        fn start() -> Self {
            use std::os::windows::process::CommandExt;
            // Walk up from the crate directory to the repository root: this crate
            // sits at different depths in the standalone repository (crates/x) and
            // in the main repository (src-tauri/crates/x), so a fixed number of
            // `parent()` hops would silently point at the wrong directory.
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            const FIXTURE_SCRIPT: &str = "scripts/tests/windows-uia-fixture.ps1";
            let repository = manifest_dir
                .ancestors()
                .find(|dir| dir.join(FIXTURE_SCRIPT).is_file())
                .unwrap_or_else(|| {
                    panic!(
                        "UIA fixture script not found: no ancestor of {} contains {FIXTURE_SCRIPT}",
                        manifest_dir.display()
                    )
                })
                .to_path_buf();
            let artifacts = repository
                .join("target/desktop-uia-fixtures")
                .join(uuid::Uuid::new_v4().simple().to_string());
            std::fs::create_dir_all(&artifacts).expect("create isolated fixture artifacts");
            let output =
                std::fs::File::create(artifacts.join("fixture.log")).expect("create fixture log");
            let errors = output.try_clone().expect("clone fixture log handle");
            let child = Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-STA",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(repository.join(FIXTURE_SCRIPT))
                .arg("-ArtifactDirectory")
                .arg(&artifacts)
                // Hide only the console; the two controlled WPF windows are
                // intentionally visible during this opt-in acceptance test.
                .creation_flags(0x0800_0000)
                .stdout(Stdio::from(output))
                .stderr(Stdio::from(errors))
                .spawn()
                .expect("launch the isolated WPF fixture");
            eprintln!("UIA fixture artifacts: {}", artifacts.display());
            Self { child, artifacts }
        }

        async fn json_when(
            &self,
            name: &str,
            predicate: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            let path = self.artifacts.join(name);
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut last = serde_json::Value::Null;
            loop {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if predicate(&value) {
                            return value;
                        }
                        last = value;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture state did not satisfy the assertion: {}; last={last}; inspect {}",
                    path.display(),
                    self.artifacts.join("fixture.log").display()
                );
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }
    }

    impl Drop for WpfFixture {
        fn drop(&mut self) {
            // Close only our child and preserve JSON/logs for diagnosis. A panic
            // cannot leave this test application running on the user's desktop.
            let _ = std::fs::write(self.artifacts.join("stop"), b"stop");
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn mouse_position() -> (i32, i32) {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }.expect("read mouse position without moving it");
        (point.x, point.y)
    }

    fn assert_background_unchanged(sentinel: HWND, mouse: (i32, i32)) {
        assert_eq!(
            unsafe { GetForegroundWindow() },
            sentinel,
            "native UIA action stole foreground focus"
        );
        assert_eq!(
            mouse_position(),
            mouse,
            "native UIA action moved the system mouse"
        );
    }

    async fn fixture_action(
        adapter: &WindowsUiaAdapter,
        scope: &ObservationScope,
        label: &str,
        kind: impl Fn(&CandidateKind) -> bool,
        input: ExecutionInput,
    ) {
        let observation = adapter
            .observe(scope)
            .await
            .expect("observe the explicit background fixture window");
        let candidates = adapter
            .build(label, &observation)
            .expect("build real UIA candidates");
        let candidate = candidates
            .iter()
            .find(|candidate| {
                kind(&candidate.kind)
                    && candidate.target.as_ref().is_some_and(|id| {
                        observation.nodes.iter().any(|node| {
                            &node.opaque_id == id && node.name.as_deref() == Some(label)
                        })
                    })
            })
            .unwrap_or_else(|| {
                panic!("fixture candidate missing for {label}; candidates={candidates:?}")
            });
        let foreground_before = unsafe { GetForegroundWindow() };
        let mouse_before = mouse_position();
        let receipt = adapter
            .execute(&observation, candidate, &input)
            .await
            .expect("execute real native UIA action");
        if receipt.dispatched {
            let foreground_after = unsafe { GetForegroundWindow() };
            let target = HWND(scope.window_handle.unwrap() as isize);
            assert!(
                foreground_after == foreground_before || foreground_after == target,
                "provider focused an unrelated window"
            );
            assert_eq!(
                receipt.delivery_mode,
                Some(if foreground_after == target {
                    DeliveryMode::Foreground
                } else {
                    DeliveryMode::Background
                }),
                "reported delivery must match real foreground behavior for {label}"
            );
            eprintln!("{label}: {:?}", receipt.delivery_mode);
            assert_eq!(mouse_position(), mouse_before, "UIA must not move mouse");
            if foreground_after != foreground_before {
                // Test setup only: restore our disposable sentinel, never an
                // arbitrary business application. No production focus masking.
                let _ = unsafe { SetForegroundWindow(foreground_before) };
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        } else {
            assert!(matches!(candidate.kind, CandidateKind::SetChecked { .. }));
        }
    }

    /// Manual machine-level smoke test. It reads whichever application is in
    /// the foreground, but deliberately does not dispatch an action.
    #[tokio::test]
    #[ignore = "requires an interactive Windows desktop with a foreground window"]
    async fn reads_real_foreground_uia_tree_and_builds_bounded_candidates() {
        let adapter = WindowsUiaAdapter::default();
        let observation = adapter
            .observe(&ObservationScope::default())
            .await
            .expect("foreground UIA observation should succeed");
        let candidates = adapter
            .build("inspect the foreground application", &observation)
            .expect("candidate construction should succeed");

        assert!(!observation.app.id.is_empty());
        assert!(!observation.window.id.is_empty());
        assert!(!observation.nodes.is_empty());
        assert!(!candidates.is_empty());
        assert!(candidates.len() <= 4096);
    }

    /// The fixture is an actual WPF application with an independent state JSON.
    /// Start only after notifying the user; never run this by default in CI.
    #[tokio::test]
    #[ignore = "opens two disposable WPF windows; requires an announced interactive Windows acceptance run"]
    #[serial_test::serial(windows_live_desktop)]
    async fn controlled_wpf_actions_report_real_delivery_and_update_application_state() {
        let fixture = WpfFixture::start();
        let ready = fixture
            .json_when("ready.json", |value| {
                value["target_hwnd"].as_i64().is_some()
            })
            .await;
        assert_eq!(
            ready["process_id"].as_u64(),
            Some(u64::from(fixture.child.id()))
        );
        let target =
            i32::try_from(ready["target_hwnd"].as_i64().unwrap()).expect("fixture target handle");
        let sentinel = HWND(ready["sentinel_hwnd"].as_i64().expect("sentinel handle") as isize);
        let foreground_deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { GetForegroundWindow() } != sentinel && Instant::now() < foreground_deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            unsafe { GetForegroundWindow() },
            sentinel,
            "fixture sentinel could not acquire foreground; no desktop actions sent"
        );
        let mouse = mouse_position();
        let scope = ObservationScope {
            window_handle: Some(target),
            delivery: DeliveryMode::Auto,
            ..Default::default()
        };
        let adapter = WindowsUiaAdapter::default();
        let initial = adapter
            .observe(&scope)
            .await
            .expect("initial fixture observation");
        let mixed = initial
            .nodes
            .iter()
            .find(|node| node.name.as_deref() == Some("Fixture checkbox"))
            .expect("three-state checkbox observed");
        assert_eq!(
            mixed.toggled, None,
            "mixed checkbox must not be reported as unchecked"
        );
        assert_background_unchanged(sentinel, mouse);

        let text = "Nuphus 后台输入 🧪";
        let offered = adapter.build("Fixture text", &initial).unwrap();
        let candidate = offered
            .iter()
            .find(|candidate| matches!(candidate.kind, CandidateKind::SetValue { .. }))
            .unwrap();
        let rejected = adapter
            .execute(
                &initial,
                candidate,
                &ExecutionInput {
                    value: Some("must not be sent".into()),
                    delivery: DeliveryMode::Background,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(rejected.to_string().contains("background_unavailable"));
        fixture
            .json_when("state.json", |state| state["text"] == "initial value")
            .await;
        assert_background_unchanged(sentinel, mouse);
        fixture_action(
            &adapter,
            &scope,
            "Fixture text",
            |kind| matches!(kind, CandidateKind::SetValue { .. }),
            ExecutionInput {
                value: Some(text.into()),
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        fixture
            .json_when("state.json", |state| state["text"] == text)
            .await;
        assert_eq!(
            mouse_position(),
            mouse,
            "native text input must not move mouse"
        );
        assert_background_unchanged(sentinel, mouse);

        fixture_action(
            &adapter,
            &scope,
            "Fixture checkbox",
            |kind| *kind == CandidateKind::SetChecked { checked: true },
            ExecutionInput {
                checked: Some(true),
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        let checked = fixture
            .json_when("state.json", |state| state["checked"] == true)
            .await;
        let change_count = checked["checkbox_changes"].as_u64().unwrap();
        fixture_action(
            &adapter,
            &scope,
            "Fixture checkbox",
            |kind| *kind == CandidateKind::SetChecked { checked: true },
            ExecutionInput {
                checked: Some(true),
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let unchanged = fixture
            .json_when("state.json", |state| state["checked"] == true)
            .await;
        assert_eq!(
            unchanged["checkbox_changes"].as_u64(),
            Some(change_count),
            "desired checkbox replay should not emit another toggle"
        );
        assert_background_unchanged(sentinel, mouse);

        fixture_action(
            &adapter,
            &scope,
            "Fixture range",
            |kind| matches!(kind, CandidateKind::SetRangeValue { .. }),
            ExecutionInput {
                value: Some("37.5".into()),
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        fixture
            .json_when("state.json", |state| state["range"].as_f64() == Some(37.5))
            .await;
        assert_background_unchanged(sentinel, mouse);

        fixture_action(
            &adapter,
            &scope,
            "Fixture items",
            |kind| {
                *kind
                    == CandidateKind::Scroll {
                        direction: ScrollDirection::Down,
                        amount: ScrollAmount::Page,
                    }
            },
            ExecutionInput {
                direction: Some(ScrollDirection::Down),
                amount: Some(ScrollAmount::Page),
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        let scrolled = fixture
            .json_when("state.json", |state| {
                state["scroll_offset"]
                    .as_f64()
                    .is_some_and(|offset| offset > 0.0)
            })
            .await;
        assert_background_unchanged(sentinel, mouse);

        fixture_action(
            &adapter,
            &scope,
            "Fixture row 47",
            |kind| *kind == CandidateKind::ScrollIntoView,
            ExecutionInput {
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        fixture
            .json_when("state.json", |state| {
                state["last_row_fully_visible"] == true
                    && state["scroll_offset"].as_f64() > scrolled["scroll_offset"].as_f64()
            })
            .await;
        assert_background_unchanged(sentinel, mouse);

        fixture_action(
            &adapter,
            &scope,
            "Fixture apply",
            |kind| *kind == CandidateKind::Invoke,
            ExecutionInput {
                delivery: DeliveryMode::Auto,
                ..Default::default()
            },
        )
        .await;
        let final_state = fixture
            .json_when("state.json", |state| state["apply_count"] == 1)
            .await;
        assert_eq!(final_state["text"], text);
        assert_eq!(final_state["checked"], true);
        assert_eq!(final_state["range"].as_f64(), Some(37.5));
        assert_eq!(
            final_state["selected_index"], -1,
            "scroll must not silently select an item"
        );
        assert_background_unchanged(sentinel, mouse);
        std::fs::write(
            fixture.artifacts.join("passed.json"),
            serde_json::to_vec_pretty(&final_state).unwrap(),
        )
        .expect("persist acceptance evidence");
    }
}
