//! The skill table a phone's command palette draws.
//!
//! A skill is a directory with a `SKILL.md` in it, installed per agent under
//! `<agent home>/skills/<name>/SKILL.md`. The phone never reads that disk — it
//! holds a cached copy and asks this host what changed, which is why the request
//! carries the fingerprints the phone already has instead of a timestamp: a
//! `touch` must not look like an edit, and a rewrite with identical text must
//! not look like a change either.
//!
//! Two rules shape the types:
//!
//! - `skills` is a **delta**. An entry whose fingerprint the phone already holds
//!   is never sent, so a thousand skills and one new one costs one entry.
//! - `complete` is the only thing that lets a phone delete. `removed` is a
//!   deletion list, and "the host could not finish reading" and "these skills
//!   are gone" are otherwise the same empty answer — see the design note in
//!   `docs/protocol-v1.md`.

use serde::{Deserialize, Serialize};

use crate::engine::EngineId;

/// What one `skill.catalog` call asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalogRequest {
    /// What the phone already holds, so the host can answer with a delta.
    ///
    /// Absent or empty means "I have nothing" — a fresh install, or a phone
    /// whose cache was cleared — and the host answers with every skill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known: Vec<KnownSkill>,
    /// The most entries the host may send back.
    ///
    /// The host clamps it to `1...=2000` and defaults to 1000. It is a ceiling
    /// on the *answer*, not on the disk: a phone that asks for 50 gets its 50
    /// and `complete: false`, so it keeps the rest of its cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_skills: Option<u32>,
}

/// One skill the phone already holds.
///
/// Only the two fields the delta needs. A phone sends its cached entries
/// verbatim, so the name and description that ride along are simply ignored
/// here rather than rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownSkill {
    pub id: String,
    pub fingerprint: String,
}

/// The host's answer to one `skill.catalog` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalogResponse {
    /// The skills that are new or changed — never the unchanged ones.
    pub skills: Vec<SkillEntry>,
    /// The skills the phone holds that this machine no longer has.
    ///
    /// Empty whenever `complete` is false: an incomplete read cannot say that a
    /// skill is gone, only that it did not see it.
    #[serde(default)]
    pub removed: Vec<String>,
    /// Whether this scan saw everything there is.
    pub complete: bool,
    /// When the scan behind this answer ran, in Unix milliseconds.
    pub scanned_at: u64,
    /// What each engine this connection may see contributed.
    pub engines: Vec<SkillEngineStatus>,
    /// Whether a cap cut this answer short.
    ///
    /// True when the scan hit `maxSkills`, or when the answer had to be trimmed
    /// to fit a frame. Either way `complete` is false.
    pub truncated: bool,
}

/// One skill, as the phone's palette lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    /// The deduped skill name, and what the phone echoes back in `known`.
    pub id: String,
    /// The name to show. Equal to `id` today: the frontmatter `name`, or the
    /// directory name when the frontmatter has none.
    pub name: String,
    /// One line, whitespace collapsed, at most 600 characters.
    pub description: String,
    /// FNV-1a 64 of `name + U+001F + description`, as 16 lowercase hex digits.
    ///
    /// Deliberately not derived from mtime or size: `touch` and an editor's
    /// save-without-change must both leave this alone.
    pub fingerprint: String,
    /// Which agent installed it.
    pub engine: EngineId,
}

/// What one engine contributed to a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEngineStatus {
    pub id: EngineId,
    /// How many skills this engine's directory held, before dedupe.
    pub count: u64,
    /// Why this engine could not be read, when it could not be.
    ///
    /// A missing directory is not an error — a machine with no skills for an
    /// agent is the normal case — so this is only set when reading started and
    /// failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_is_all_optional() {
        // The phone sends `{}` when it has nothing, and must not have to.
        let request: SkillCatalogRequest = serde_json::from_value(json!({})).unwrap();
        assert!(request.known.is_empty());
        assert_eq!(request.max_skills, None);

        let full: SkillCatalogRequest = serde_json::from_value(json!({
            "known": [{ "id": "grilling", "fingerprint": "9f2a", "name": "grilling" }],
            "maxSkills": 50,
        }))
        .unwrap();
        assert_eq!(full.known[0].id, "grilling");
        assert_eq!(full.known[0].fingerprint, "9f2a");
        assert_eq!(full.max_skills, Some(50));
    }

    #[test]
    fn the_answer_speaks_camel_case_and_omits_only_what_is_absent() {
        let answer = SkillCatalogResponse {
            skills: vec![SkillEntry {
                id: "grilling".to_string(),
                name: "grilling".to_string(),
                description: "Grill a plan".to_string(),
                fingerprint: "0123456789abcdef".to_string(),
                engine: EngineId::Dsh,
            }],
            removed: vec!["handoff".to_string()],
            complete: true,
            scanned_at: 1_763_351_700_123,
            engines: vec![
                SkillEngineStatus {
                    id: EngineId::Dsh,
                    count: 1,
                    error: None,
                },
                SkillEngineStatus {
                    id: EngineId::Claude,
                    count: 0,
                    error: Some("permission denied".to_string()),
                },
            ],
            truncated: false,
        };
        let value = serde_json::to_value(&answer).unwrap();
        assert_eq!(value["scannedAt"], 1_763_351_700_123u64);
        assert_eq!(value["skills"][0]["engine"], "dsh");
        assert_eq!(value["engines"][0]["count"], 1);
        // "no error" is an absent key, not a `null` a client has to know about.
        assert!(value["engines"][0].get("error").is_none());
        assert_eq!(value["engines"][1]["error"], "permission denied");

        // A request round-trips to the shape the design doc prints.
        let request = SkillCatalogRequest {
            known: Vec::new(),
            max_skills: None,
        };
        assert_eq!(serde_json::to_value(&request).unwrap(), json!({}));
    }
}
