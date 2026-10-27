//! Shaping DSH's catalogs into the frames a phone's pickers draw.
//!
//! A direct port of `dsh` v0.8.12 `src/bridge/models.ts` and `permissions.ts`.
//! The rules that matter, all learned against real hosts:
//!
//! - The catalog carries no context window. Capacity is known per session, from
//!   the usage frame, so the model row must not claim one.
//! - A model's reasoning levels are the provider's, in its order, and sending an
//!   effort the model does not list is a hard error upstream rather than
//!   something clamped. A model with none must offer none.
//! - `auto` is live-only and `custom` is derived-only; the catalog is passed
//!   through rather than curated.

use agi_protocol::catalog::{
    CatalogFailure, ModelCatalog, ModelOption, PermissionCatalog, PermissionOption, ProviderGroup,
    ReasoningEffort,
};
use agi_protocol::usage::ModelSelection;
use serde_json::Value;

/// Read a string field.
fn text<'a>(source: Option<&'a Value>, key: &str) -> Option<&'a str> {
    source?.get(key)?.as_str()
}

/// Narrow one model row.
fn to_model(value: &Value) -> Option<ModelOption> {
    let id = value.get("id")?.as_str()?;
    let reasoning = value.get("reasoning");
    let efforts = reasoning
        .and_then(|entry| entry.get("efforts"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?;
                    Some(ReasoningEffort {
                        id: id.to_string(),
                        name: row
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_string(),
                        description: row
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ModelOption {
        id: id.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        efforts,
        default_effort: text(reasoning, "defaultEffort").map(str::to_string),
    })
}

/// Narrow a route.
fn to_selection(value: Option<&Value>) -> Option<ModelSelection> {
    let provider = text(value, "provider")?;
    let model = text(value, "model")?;
    Some(ModelSelection {
        provider: provider.to_string(),
        model: model.to_string(),
        reasoning_effort: text(value, "reasoningEffort").map(str::to_string),
    })
}

/// Shape the answer to `session/modelCatalog`.
pub fn model_catalog_from(value: &Value) -> ModelCatalog {
    let providers = value
        .get("groups")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .filter_map(|group| {
                    let id = group.get("id")?.as_str()?;
                    let models = group
                        .get("models")
                        .and_then(Value::as_array)
                        .map(|rows| rows.iter().filter_map(to_model).collect())
                        .unwrap_or_default();
                    Some(ProviderGroup {
                        id: id.to_string(),
                        name: group
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_string(),
                        models,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let failures = value
        .get("failures")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?;
                    Some(CatalogFailure {
                        id: id.to_string(),
                        name: row
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_string(),
                        message: row
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unavailable")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    ModelCatalog {
        default: to_selection(value.get("default")),
        providers,
        failures,
    }
}

/// Narrow one preset option.
fn to_option(value: &Value) -> Option<PermissionOption> {
    let id = value.get("value")?.as_str()?;
    Some(PermissionOption {
        value: id.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Shape the answer to `permissionPresets/catalog`.
pub fn permission_catalog_from(value: &Value) -> PermissionCatalog {
    let narrow = |key: &str| -> Vec<PermissionOption> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(to_option).collect())
            .unwrap_or_default()
    };
    PermissionCatalog {
        options: narrow("options"),
        defaults: narrow("defaultOptions"),
        default_preset: value
            .get("defaultPreset")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes_the_catalog_the_app_renders() {
        let catalog = model_catalog_from(&json!({
            "default": {"provider": "opencode-go", "model": "v4", "reasoningEffort": "max"},
            "groups": [{
                "id": "opencode-go",
                "models": [{
                    "id": "deepseek-v4.1-flash",
                    "name": "DeepSeek V4 Flash",
                    "reasoning": {"efforts": [{"id": "max", "name": "Max"}], "defaultEffort": "high"},
                }, {"id": "second"}],
            }],
            "failures": [{"id": "broken", "message": "timed out"}],
        }));
        assert_eq!(catalog.providers.len(), 1);
        let group = &catalog.providers[0];
        assert_eq!(group.name, "opencode-go");
        assert_eq!(group.models[0].name, "DeepSeek V4 Flash");
        assert_eq!(group.models[0].efforts[0].id, "max");
        assert_eq!(group.models[0].default_effort.as_deref(), Some("high"));
        assert_eq!(group.models[1].name, "second");
        assert!(group.models[1].efforts.is_empty());
        assert_eq!(catalog.failures[0].name, "broken");
        assert_eq!(catalog.failures[0].message, "timed out");
        assert_eq!(catalog.default.as_ref().unwrap().model, "v4");
    }

    #[test]
    fn an_empty_catalog_is_an_empty_catalog_not_a_failure() {
        let catalog = model_catalog_from(&json!({}));
        assert!(catalog.providers.is_empty());
        assert!(catalog.failures.is_empty());
        assert!(catalog.default.is_none());
    }

    #[test]
    fn shapes_the_permission_catalog() {
        let catalog = permission_catalog_from(&json!({
            "options": [{"value": "workspace-write", "name": "Workspace write"}],
            "defaultOptions": [{"value": "read-only", "name": "Read only"}],
            "defaultPreset": "workspace-write",
        }));
        assert_eq!(catalog.options[0].value, "workspace-write");
        assert_eq!(catalog.defaults[0].name, "Read only");
        assert_eq!(catalog.default_preset, "workspace-write");
    }

    #[test]
    fn a_preset_without_a_value_is_not_an_option() {
        let catalog = permission_catalog_from(&json!({
            "options": [{"name": "nameless"}, {"value": "ok", "name": "Ok"}],
        }));
        assert_eq!(catalog.options.len(), 1);
        assert_eq!(catalog.options[0].value, "ok");
        assert_eq!(catalog.default_preset, "");
    }
}
