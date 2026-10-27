//! The three engines this application can host.
//!
//! An engine is a *product*, not a process: DSH may be one process, Claude Code
//! spawns a child per turn, Codex runs a long-lived app-server. What they share
//! is that a person talks to them in turns, and that is the only thing the rest
//! of this workspace assumes.

use serde::{Deserialize, Serialize};

/// Which agent a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineId {
    /// DeepSeek Harness, reached through the `dsh-mizar` helper plugin.
    Dsh,
    /// Claude Code, reached through hooks and `claude -p`.
    Claude,
    /// Codex, reached through `codex app-server`.
    Codex,
}

impl EngineId {
    /// The stable lowercase name, also the prefix of a [`crate::SessionKey`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dsh => "dsh",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Parse the lowercase name.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dsh" => Some(Self::Dsh),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The product name a person reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dsh => "DSH",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// Every engine, in the order the UI shows them.
    pub fn all() -> [Self; 3] {
        [Self::Dsh, Self::Claude, Self::Codex]
    }
}

impl std::fmt::Display for EngineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one engine can do, as far as its adapter knows.
///
/// The UI renders from this rather than from "did the call fail": a button that
/// cannot work is not drawn, which is the difference between a product that
/// looks finished on all three engines and one that reports errors on two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Send files or images with a prompt.
    pub attachments: bool,
    /// Read a file the model delivered.
    pub file_preview: bool,
    /// Branch a conversation at a turn.
    pub fork: bool,
    /// Hide a session without deleting it.
    pub archive: bool,
    /// A live feed of what the running turn is doing.
    pub activity: bool,
    /// The provider account's balance.
    pub balance: bool,
    /// Pick a model and reasoning effort.
    pub models: bool,
    /// Pick a permission preset.
    pub permissions: bool,
}

impl Capabilities {
    /// Everything the protocol can express. Adapters narrow this.
    pub const ALL: Self = Self {
        attachments: true,
        file_preview: true,
        fork: true,
        archive: true,
        activity: true,
        balance: true,
        models: true,
        permissions: true,
    };

    /// Nothing but reading and prompting.
    pub const MINIMAL: Self = Self {
        attachments: false,
        file_preview: false,
        fork: false,
        archive: false,
        activity: false,
        balance: false,
        models: false,
        permissions: false,
    };
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::MINIMAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for engine in EngineId::all() {
            assert_eq!(EngineId::parse(engine.as_str()), Some(engine));
        }
        assert_eq!(EngineId::parse("DSH"), None);
    }

    #[test]
    fn labels_are_product_names() {
        assert_eq!(EngineId::Claude.label(), "Claude Code");
        assert_eq!(EngineId::Dsh.label(), "DSH");
    }
}
