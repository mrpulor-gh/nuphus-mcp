//! Platform-independent semantic desktop automation core.
//!
//! This module intentionally contains no platform handles or coordinates.
//! Platform adapters produce observations and execute locally validated
//! candidates; decision providers may only return a candidate id.

mod macos_accessibility;
mod outcome;
#[cfg(any(target_os = "windows", target_os = "macos", test))]
mod risk;
mod runner;
pub mod targets;
mod types;
pub mod verification;
mod windows_uia;

pub use macos_accessibility::MacosAccessibilityAdapter;
pub use outcome::{ActionEffect, DesktopActionError, DispatchState};
#[cfg(any(target_os = "windows", target_os = "macos", test))]
pub(crate) use risk::classify_desktop_risk;
pub use runner::{
    AutomationRunner, CandidateBuilder, ComputerExecutor, ComputerObserver, Verifier,
};
pub use targets::{register_user_application, DesktopTargetDescriptor, DesktopTargetService};
pub use types::*;
pub use verification::{CompletionPolicy, DesktopExpectation, StateCondition, StateStatus};
pub use windows_uia::WindowsUiaAdapter;
