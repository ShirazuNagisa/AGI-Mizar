//! Agent interconnect, local route: the commands an agent on this computer
//! calls to hand work to the DSH beside it.
//!
//! One test function, because `Core::new` reads the process environment
//! (`MIZAR_DATA_DIR`, `AGI_ACCOUNT_URL`) and a second test in the same binary
//! would race the first. The engine is a fake that answers in one turn and
//! remembers whether it was archived; everything above it — the delegation
//! record, the cancel signal, the settle-time archive, the refusal to continue
//! an archived session — is the real kernel.

use std::sync::{Arc, Mutex};

use agi_core::Core;
use agi_engine::{Answer, Engine, EngineError, EngineEvent};
use agi_protocol::api::{Command, CommandResponse};
use agi_protocol::catalog::{ModelCatalog, PermissionCatalog, PermissionOption, ProviderGroup};
use agi_protocol::domain::{Record, RecordKind, Session, SessionKey, Workspace};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use serde_json::Value;

/// One DSH-shaped engine that answers every prompt with `answer`.
struct FakeDsh {
    answer: String,
    events: tokio::sync::broadcast::Sender<EngineEvent>,
    records: Mutex<Vec<Record>>,
    turn: Mutex<u32>,
    archived: Mutex<Vec<String>>,
    /// When set, `create` fails with this message.
    broken: bool,
}

impl FakeDsh {
    fn new(answer: &str) -> Arc<Self> {
        let (events, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Self {
            answer: answer.to_string(),
            events,
            records: Mutex::new(Vec::new()),
            turn: Mutex::new(0),
            archived: Mutex::new(Vec::new()),
            broken: false,
        })
    }
}

fn record(seq: i64, turn: u32, kind: RecordKind, text: Option<&str>) -> Record {
    Record {
        seq,
        time: seq,
        turn: Some(turn),
        step: None,
        kind,
        text: text.map(str::to_string),
        tool: None,
    }
}

#[async_trait]
impl Engine for FakeDsh {
    fn id(&self) -> EngineId {
        EngineId::Dsh
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            attachments: false,
            file_preview: false,
            fork: false,
            archive: true,
            activity: false,
            balance: false,
            models: true,
            permissions: true,
        }
    }

    fn status(&self) -> (bool, Option<String>) {
        (true, Some("fake dsh".to_string()))
    }

    async fn sessions(&self) -> Result<Vec<Session>, EngineError> {
        Ok(Vec::new())
    }

    async fn records(
        &self,
        _session: &SessionKey,
        _limit: usize,
        _before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        Ok(self.records.lock().unwrap().clone())
    }

    async fn prompt(&self, session: &SessionKey, _text: &str) -> Result<(), EngineError> {
        let turn = {
            let mut turn = self.turn.lock().unwrap();
            *turn += 1;
            *turn
        };
        let base = i64::from(turn) * 3 - 2;
        let rows = [
            record(base, turn, RecordKind::TurnStart, None),
            record(
                base + 1,
                turn,
                RecordKind::AssistantMessage,
                Some(&self.answer),
            ),
            record(base + 2, turn, RecordKind::TurnEnd, Some("completed")),
        ];
        {
            let mut records = self.records.lock().unwrap();
            records.extend(rows.clone());
        }
        // The same two events the real DSH adapter emits: a record per row and
        // a session summary whose `running` flag follows the turn.
        let mut running = Session::bare(session.clone(), 1);
        running.running = true;
        let _ = self.events.send(EngineEvent::SessionUpdated(running));
        for row in rows {
            let end = row.kind == RecordKind::TurnEnd;
            let _ = self.events.send(EngineEvent::RecordAppended {
                session: session.clone(),
                record: row,
            });
            if end {
                let mut idle = Session::bare(session.clone(), 2);
                idle.running = false;
                let _ = self.events.send(EngineEvent::SessionUpdated(idle));
            }
        }
        Ok(())
    }

    async fn cancel(&self, _session: &SessionKey) -> Result<(), EngineError> {
        Ok(())
    }

    async fn answer(&self, _id: &str, _answer: Answer) -> Result<(), EngineError> {
        Ok(())
    }

    async fn set_model(&self, _session: &SessionKey, _model: &str) -> Result<(), EngineError> {
        Ok(())
    }

    async fn set_permission(
        &self,
        _session: &SessionKey,
        _preset: &str,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    async fn create(&self, _cwd: Option<&str>) -> Result<SessionKey, EngineError> {
        if self.broken {
            return Err(EngineError::Upstream {
                code: "NO_DSH".to_string(),
                message: "the fake DSH is not running".to_string(),
                details: None,
            });
        }
        Ok(SessionKey::new(EngineId::Dsh, "local-1"))
    }

    async fn archive(&self, session: &SessionKey, _stop: bool) -> Result<(), EngineError> {
        self.archived.lock().unwrap().push(session.to_string());
        Ok(())
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, EngineError> {
        Ok(vec![Workspace {
            id: "0".to_string(),
            name: Some("demo".to_string()),
            path: "/tmp/demo".to_string(),
            session_ids: None,
        }])
    }

    async fn models(&self) -> Result<Vec<agi_protocol::domain::ModelInfo>, EngineError> {
        Ok(Vec::new())
    }

    async fn permissions(
        &self,
    ) -> Result<Vec<agi_protocol::domain::PermissionPreset>, EngineError> {
        Ok(Vec::new())
    }

    async fn model_catalog(&self) -> Result<ModelCatalog, EngineError> {
        Ok(ModelCatalog {
            default: Some(agi_protocol::usage::ModelSelection {
                provider: "deepseek".to_string(),
                model: "v4".to_string(),
                reasoning_effort: Some("high".to_string()),
            }),
            providers: vec![ProviderGroup {
                id: "deepseek".to_string(),
                name: "DeepSeek".to_string(),
                models: Vec::new(),
            }],
            failures: Vec::new(),
        })
    }

    async fn permission_catalog(&self) -> Result<PermissionCatalog, EngineError> {
        Ok(PermissionCatalog {
            default_preset: "workspace-write".to_string(),
            defaults: vec![PermissionOption {
                value: "workspace-write".to_string(),
                name: "Workspace write".to_string(),
                description: None,
            }],
            options: vec![PermissionOption {
                value: "workspace-write".to_string(),
                name: "Workspace write".to_string(),
                description: None,
            }],
        })
    }
}

fn ok(response: CommandResponse) -> Value {
    assert!(response.ok, "{:?}", response.error);
    response.value.unwrap_or(Value::Null)
}

fn error_code(response: CommandResponse) -> String {
    response.error.map(|error| error.code).unwrap_or_default()
}

/// Wait until a delegation stops running, then read it back.
async fn settled(core: &Arc<Core>, id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the local delegation did not settle"
        );
        let value = ok(core
            .dispatch(Command::RemoteResult { id: id.to_string() })
            .await);
        if value["delegation"]["state"] != "running" {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_local_subagent_is_delegated_archived_and_then_refuses_follow_ups() {
    let engine = FakeDsh::new("LOCAL-OK");
    let core = Core::new(vec![Arc::clone(&engine) as Arc<dyn Engine>]);

    // What an agent asks before choosing a workspace and a preset.
    let options = ok(core.dispatch(Command::LocalOptions).await);
    assert_eq!(options["available"], true);
    assert_eq!(options["autoArchive"], true, "on by default");
    assert_eq!(options["workspaces"][0]["path"], "/tmp/demo");
    assert_eq!(options["permissions"]["defaultPreset"], "workspace-write");
    assert_eq!(options["models"]["default"]["provider"], "deepseek");

    let started = ok(core
        .dispatch(Command::LocalDelegate {
            cwd: Some("/tmp/demo".to_string()),
            permission: Some("workspace-write".to_string()),
            provider: Some("deepseek".to_string()),
            model: Some("v4".to_string()),
            reasoning_effort: Some("high".to_string()),
            prompt: "write the report".to_string(),
        })
        .await);
    let id = started["delegation"]["id"].as_str().unwrap().to_string();
    assert_eq!(started["delegation"]["route"], "local");
    assert_eq!(started["delegation"]["peer"], "local");

    let result = settled(&core, &id).await;
    assert_eq!(result["delegation"]["state"], "done");
    assert_eq!(result["finalText"], "LOCAL-OK");
    assert_eq!(result["delegation"]["sessionId"], "dsh:local-1");
    assert_eq!(result["delegation"]["archived"], true);
    assert_eq!(
        *engine.archived.lock().unwrap(),
        vec!["dsh:local-1".to_string()],
        "the session was archived once, where it ran"
    );

    // An archived session's replies never come back, so a follow-up is refused
    // with the reason instead of silently answering nothing.
    let refused = core
        .dispatch(Command::LocalSend {
            id: id.clone(),
            message: "one more thing".to_string(),
        })
        .await;
    assert_eq!(error_code(refused), "REMOTE_SESSION_ARCHIVED");

    // With the switch off the session stays, and a follow-up continues it.
    let prefs = core.remote_prefs();
    core.set_remote_prefs(agi_core::RemotePrefs {
        auto_archive: false,
        ..prefs
    });
    let second = ok(core
        .dispatch(Command::LocalDelegate {
            cwd: None,
            permission: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            prompt: "first step".to_string(),
        })
        .await);
    let second_id = second["delegation"]["id"].as_str().unwrap().to_string();
    let first = settled(&core, &second_id).await;
    assert_eq!(first["delegation"]["archived"], false);
    let follow = ok(core
        .dispatch(Command::LocalSend {
            id: second_id.clone(),
            message: "second step".to_string(),
        })
        .await);
    assert_eq!(
        follow["continuedFrom"],
        serde_json::json!(second_id.clone())
    );
    let follow_id = follow["delegation"]["id"].as_str().unwrap().to_string();
    let done = settled(&core, &follow_id).await;
    assert_eq!(done["finalText"], "LOCAL-OK");
    assert_eq!(done["delegation"]["archived"], false);
    // The follow-up reused the session instead of creating a second one.
    assert_eq!(
        engine.archived.lock().unwrap().len(),
        1,
        "only the first, archived session was touched"
    );
}
