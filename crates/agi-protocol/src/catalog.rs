//! The catalogs a phone's pickers draw.
//!
//! Ported from `dsh` v0.8.12 `src/bridge/models.ts` and `permissions.ts`: the
//! app renders whatever the host lists and never hard-codes a model or a preset.
//! Two consequences of that are load-bearing here:
//!
//! - `auto` appears in the permission catalog only while the auto-review
//!   integration is composed, and `custom` is a current value that is never a
//!   selectable row. The list is passed through rather than curated.
//! - A model's reasoning levels are the provider's, in its order, and sending an
//!   effort the model does not list is a hard error upstream rather than
//!   something clamped. A model with no levels must offer none.

use serde::{Deserialize, Serialize};

use crate::usage::ModelSelection;

/// The model catalog the app renders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    /// The route a new session would use.
    pub default: Option<ModelSelection>,
    /// Every provider with at least one model.
    pub providers: Vec<ProviderGroup>,
    /// Providers that failed to enumerate; shown rather than hidden.
    pub failures: Vec<CatalogFailure>,
}

/// One provider and its models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderGroup {
    pub id: String,
    pub name: String,
    pub models: Vec<ModelOption>,
}

/// One selectable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The reasoning levels this model accepts, in the provider's order.
    pub efforts: Vec<ReasoningEffort>,
    /// The level the provider applies when none is chosen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// One reasoning level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffort {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A provider that could not be listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogFailure {
    pub id: String,
    pub name: String,
    pub message: String,
}

/// The permission presets the app offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionCatalog {
    /// Presets a session may be switched to.
    pub options: Vec<PermissionOption>,
    /// Presets offered for future sessions' default.
    pub defaults: Vec<PermissionOption>,
    /// The preset a new session would use.
    pub default_preset: String,
}

/// One selectable preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// The preset id passed back when switching.
    pub value: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
