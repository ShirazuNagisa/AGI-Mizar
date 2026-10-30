//! Turn-level layout judgement for the Mizar kernel.
//!
//! One question: **did the user's request for this turn actually get finished?**
//! Four answers, because "the agent asked you something" is not a failure:
//!
//! | state | meaning | phone |
//! |---|---|---|
//! | `done` | request resolved, result handed back | normal end |
//! | `waiting` | not finished, but a subagent/job will wake the model | keep the turn open |
//! | `needs_user` | the agent stopped to ask you something | highlight |
//! | `stalled` | not finished and nothing will wake it | "this turn did not complete" |
//!
//! ### Two layers, on purpose
//!
//! Structure decides what structure can decide, and only then does a model get
//! asked. A turn that ended `error`/`aborted`, or that still has a subagent or
//! background job running, is answered by [`gate`] in microseconds — the model
//! is never invoked, because it cannot know more than the event log does.
//!
//! The model is only asked about *structurally idle* turns. That is both cheaper
//! and better for accuracy: those are the only turns where the answer is
//! genuinely semantic.
//!
//! ### Shadow mode first
//!
//! [`LayoutService`] is written to be safe to call on every turn while the UI
//! still ignores the result: it never panics, never blocks the caller, and falls
//! back to [`RuleJudge`] on any model failure ([`Source::Rule`] marks that in the
//! output so the logs can tell the two apart).

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use agi_protocol::SessionState;

pub mod model;
pub mod states;
pub use states::StateMachine;
pub use model::{Backend, LabelMap, ManifestError, ManifestJudge, ModelManifest, ModelRegistry};

use serde::{Deserialize, Serialize};

/// What the phone should show for one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Done,
    NeedsUser,
    Stalled,
    Waiting,
}

impl State {
    /// The wire name, also the token the model is trained to emit.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Done => "done",
            State::NeedsUser => "needs_user",
            State::Stalled => "stalled",
            State::Waiting => "waiting",
        }
    }

    /// Every state, in the order the model was trained with them.
    pub const ALL: [State; 4] = [
        State::Done,
        State::NeedsUser,
        State::Stalled,
        State::Waiting,
    ];
}

/// Where a judgement came from. Kept in the payload so a wrong answer can be
/// traced back to the layer that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Decided by the structural gate, no model involved.
    Gate,
    /// Decided by the model.
    Model,
    /// Model unavailable or unparsable; the rule fallback answered.
    Rule,
}

/// The judgement written into `turn/end`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// Schema version of this payload.
    pub v: u32,
    pub state: State,
    pub source: Source,
    /// Which turn it belongs to, so a stale result can be dropped.
    pub turn: Option<u32>,
}

impl Layout {
    /// Current payload version.
    pub const VERSION: u32 = 2;

    pub fn new(turn: Option<u32>, state: State, source: Source) -> Self {
        Self {
            v: Self::VERSION,
            state,
            source,
            turn,
        }
    }
}

/// Everything the judge is allowed to see about one turn.
///
/// Mirrors the training-time record exactly; field order here is irrelevant but
/// the *rendered* string is not — [`render_prompt`] must stay byte-identical to
/// the Python/JS pipeline that produced the training data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Facts {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub tools: String,
    #[serde(default)]
    pub failures: Vec<String>,
    #[serde(default)]
    pub minutes: f64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub steps: u32,
    #[serde(default, rename = "replyCount")]
    pub reply_count: u32,
    #[serde(default)]
    pub texts: Vec<String>,
    #[serde(default, rename = "endsWithQuestion")]
    pub ends_with_question: bool,
    #[serde(default)]
    pub subagents: Vec<String>,
    #[serde(default)]
    pub jobs: Vec<String>,
}

impl Facts {
    /// Whether the turn still has something that will wake the model.
    pub fn suspended(&self) -> bool {
        !self.subagents.is_empty() || !self.jobs.is_empty()
    }

    fn asked_with_tool(&self) -> bool {
        let lower = self.tools.to_ascii_lowercase();
        lower.contains("ask_user") || lower.contains("askuserquestion")
    }

    /// The `本轮事实` line: facts the pipeline already knows, handed to the model
    /// instead of making it infer them from prose.
    pub fn signals(&self) -> String {
        format!(
            "结尾是问句={} · 用过提问工具={} · 失败调用={} · 结束原因={} · 回复={} 条 · 步数={}",
            yes_no(self.ends_with_question),
            yes_no(self.asked_with_tool()),
            self.failures.len(),
            self.reason,
            self.reply_count,
            self.steps
        )
    }
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "是"
    } else {
        "否"
    }
}

/// JS `Number#toString` for the values we actually have (one decimal at most).
fn fmt_number(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v}");
        s
    }
}

/// Render the model's user message. **Must match the training pipeline byte for byte.**
///
/// The body of this function mirrors `tools/teacher/format.mjs`; `tests/parity.rs`
/// pins the two together with a fixture captured from the JS side.
pub fn render_prompt(f: &Facts) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(12);
    parts.push(format!("本轮事实：{}", f.signals()));
    parts.push(String::new());
    parts.push("用户指令：".to_string());
    parts.push(if f.user.is_empty() {
        "（空）".to_string()
    } else {
        f.user.clone()
    });
    parts.push(String::new());
    parts.push(format!(
        "本轮工具：{}",
        if f.tools.is_empty() { "（无）" } else { &f.tools }
    ));

    if !f.failures.is_empty() {
        parts.push("失败的调用：".to_string());
        for x in &f.failures {
            parts.push(format!("- {x}"));
        }
    }
    if !f.subagents.is_empty() {
        parts.push("仍在运行的子代理：".to_string());
        for x in &f.subagents {
            parts.push(format!("- {x}"));
        }
    }
    if !f.jobs.is_empty() {
        parts.push("仍在运行的后台任务：".to_string());
        for x in &f.jobs {
            parts.push(format!("- {x}"));
        }
    }

    parts.push(String::new());
    parts.push(format!(
        "轮次信息：{} 分钟 · {} 步 · 结束原因 {} · 共 {} 条回复",
        fmt_number(f.minutes),
        f.steps,
        f.reason,
        f.reply_count
    ));
    parts.push(String::new());
    parts.push(format!(
        "模型最后的话（按顺序，共 {} 条）：",
        f.texts.len()
    ));
    for (i, t) in f.texts.iter().enumerate() {
        let mut head = String::new();
        let _ = write!(head, "--- 第 {} 条 ---", i + 1);
        parts.push(head);
        parts.push(t.clone());
    }
    parts.push(String::new());
    parts.push("只输出 JSON。".to_string());

    parts.join("\n")
}

/// Pull a state out of whatever the model produced.
///
/// Accepts `{"state":"done"}`, a bare `done`, or a ` thinking…<｜end▁of▁thinking｜>` wrapper —
/// the model sometimes thinks briefly before answering, and rejecting that would
/// cost the whole turn.
pub fn parse_state(text: &str) -> Option<State> {
    for st in State::ALL {
        let quoted = format!("\"state\"");
        let _ = quoted;
        let needle = format!("\"{}\"", st.as_str());
        if text.contains(&needle) {
            return Some(st);
        }
    }
    // fall back to a bare word, longest first so `needs_user` wins over `user`
    let lower = text.to_ascii_lowercase();
    let mut best: Option<(usize, State)> = None;
    for st in State::ALL {
        if let Some(pos) = lower.rfind(st.as_str()) {
            if best.map(|(p, _)| pos > p).unwrap_or(true) {
                best = Some((pos, st));
            }
        }
    }
    best.map(|(_, st)| st)
}

/// The structural half of the decision.
///
/// Returns `Some` when the event log already answers the question, `None` when
/// the turn is idle and only the model can say.
pub fn gate(f: &Facts) -> Option<State> {
    match f.reason.as_str() {
        "error" | "aborted" | "interrupted" => return Some(State::Stalled),
        _ => {}
    }
    if f.suspended() {
        return Some(State::Waiting);
    }
    None
}

/// One ordered item of a turn, as the integration layer sees it.
///
/// The kernel maps its own `Record`s onto this tiny view so the assembly rules
/// (which text is the user's instruction, how tools are summarised, how many
/// messages the judge sees) stay here, where they are tested, instead of being
/// re-derived in the core.
#[derive(Debug, Clone)]
pub enum Item<'a> {
    User(&'a str),
    Assistant(&'a str),
    Tool(&'a str),
    ToolFailed(&'a str),
    Step,
    TurnEnd(&'a str),
}

/// Longest tail a single message keeps; matches the training clips.
const TEXT_CLIP: usize = 1200;
const FAILURE_CLIP: usize = 160;
/// How many of the model's last messages the judge reads.
const TAIL_TEXTS: usize = 3;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Assemble [`Facts`] from a turn's items.
///
/// `minutes` is supplied by the caller because only it knows the record clock.
pub fn facts_from_turn<'a>(
    id: &str,
    minutes: f64,
    items: impl IntoIterator<Item = Item<'a>>,
) -> Facts {
    let mut f = Facts {
        id: id.to_string(),
        minutes,
        reason: "completed".to_string(),
        ..Facts::default()
    };
    let mut texts: Vec<String> = Vec::new();
    let mut tools: Vec<(String, usize)> = Vec::new();

    for item in items {
        match item {
            Item::User(t) => f.user = t.to_string(),
            Item::Assistant(t) => {
                f.reply_count += 1;
                texts.push(t.to_string());
            }
            Item::Tool(name) => match tools.last_mut() {
                Some((last, n)) if last == name => *n += 1,
                _ => tools.push((name.to_string(), 1)),
            },
            Item::ToolFailed(body) => f.failures.push(clip(body, FAILURE_CLIP)),
            Item::Step => f.steps += 1,
            Item::TurnEnd(reason) => f.reason = reason.to_string(),
        }
    }

    f.tools = tools
        .iter()
        .map(|(name, n)| {
            if *n > 1 {
                format!("{name}×{n}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    f.failures.truncate(3);

    let tail = texts.len().saturating_sub(TAIL_TEXTS);
    f.texts = texts[tail..].iter().map(|t| clip(t, TEXT_CLIP)).collect();
    f.ends_with_question = f
        .texts
        .last()
        .map(|t| {
            let t = t.trim_end();
            t.ends_with('?') || t.ends_with('？')
        })
        .unwrap_or(false);
    f
}

/// What the live record stream says a session is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// No turn in flight.
    Idle,
    /// The model is reasoning; nothing has come out yet.
    Thinking,
    /// Assistant text is streaming.
    Writing,
    /// A tool call is in flight.
    Tool,
}

impl Live {
    /// The state code for a live signal.
    pub fn state(self) -> SessionState {
        match self {
            Live::Idle => SessionState::Idle,
            Live::Thinking => SessionState::Thinking,
            Live::Writing => SessionState::StreamNarration,
            Live::Tool => SessionState::Tool,
        }
    }
}

/// The finished-turn state.
///
/// Structure wins where it is factual (an error is an error, a running subagent
/// is a running subagent); the model decides only the semantic case, and only
/// the manifest decides which code it lands on. Nothing here guesses.
pub fn state_of_turn(facts: &Facts, verdict: Option<State>) -> SessionState {
    match facts.reason.as_str() {
        "error" | "failed" | "failure" => return SessionState::Failed,
        "aborted" | "interrupted" | "cancelled" | "canceled" => return SessionState::Paused,
        _ => {}
    }
    if !facts.subagents.is_empty() {
        return SessionState::Subagent;
    }
    if !facts.jobs.is_empty() {
        return SessionState::Background;
    }
    match verdict {
        Some(State::Done) => SessionState::Completed,
        Some(State::NeedsUser) => SessionState::AwaitingUser,
        Some(State::Stalled) => SessionState::Failed,
        Some(State::Waiting) => SessionState::Background,
        // No model: the same conservative rules the fallback judge uses.
        None => {
            if facts.ends_with_question || facts.asked_with_tool() {
                SessionState::AwaitingUser
            } else if facts.texts.is_empty() {
                SessionState::Failed
            } else {
                SessionState::Completed
            }
        }
    }
}

/// A judge turns facts into a state. Implementations must be cheap to call and
/// must not panic — a failed judgement degrades the UI, not the kernel.
#[async_trait]
pub trait Judge: Send + Sync {
    async fn judge(&self, facts: &Facts) -> Option<State>;
}

/// No model: the gate plus two honest heuristics.
///
/// Used when the GGUF is missing, the sidecar is down, or the model answered
/// with something unparsable. It is deliberately conservative — it never claims
/// a turn is `stalled` without structural evidence, because a false "did not
/// finish" is the one error users cannot verify.
pub struct RuleJudge;

#[async_trait]
impl Judge for RuleJudge {
    async fn judge(&self, facts: &Facts) -> Option<State> {
        if let Some(g) = gate(facts) {
            return Some(g);
        }
        if facts.ends_with_question || facts.asked_with_tool() {
            return Some(State::NeedsUser);
        }
        if !facts.texts.is_empty() {
            return Some(State::Done);
        }
        Some(State::Stalled)
    }
}

/// Talks to a warm `llama-server` sidecar over loopback.
///
/// The model stays loaded in the sidecar, so one judgement is a single HTTP round
/// trip instead of a process spawn. That is the difference between 50 ms and 2 s.
pub struct LlamaServerJudge {
    /// e.g. `http://127.0.0.1:8791`
    pub endpoint: String,
    /// Optional API key, for a sidecar that asks for one.
    pub api_key: Option<String>,
}

#[async_trait]
impl Judge for LlamaServerJudge {
    async fn judge(&self, facts: &Facts) -> Option<State> {
        let prompt = render_prompt(facts);
        let body = serde_json::json!({
            "prompt": prompt,
            "n_predict": 24,
            "temperature": 0.0,
            "stop": ["\n\n"],
            "cache_prompt": true,
        });
        let client = reqwest::Client::new();
        let mut req = client
            .post(format!("{}/completion", self.endpoint.trim_end_matches('/')))
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        let text = json.get("content")?.as_str()?;
        parse_state(text)
    }
}

/// Gate first, model second, rules last.
pub struct LayoutService {
    judge: Arc<dyn Judge>,
}

impl LayoutService {
    /// Build a service over any judge. The kernel passes a
    /// [`LlamaServerJudge`]; tests pass [`RuleJudge`].
    pub fn new(judge: Arc<dyn Judge>) -> Self {
        Self { judge }
    }

    /// Judge one turn. Never fails: the worst case is [`Source::Rule`].
    pub async fn judge_turn(&self, turn: Option<u32>, facts: &Facts) -> Layout {
        if let Some(state) = gate(facts) {
            return Layout::new(turn, state, Source::Gate);
        }
        if let Some(state) = self.judge.judge(facts).await {
            return Layout::new(turn, state, Source::Model);
        }
        let state = RuleJudge.judge(facts).await.unwrap_or(State::Done);
        Layout::new(turn, state, Source::Rule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Facts {
        Facts {
            id: "s:1".into(),
            user: "把 README 更新一下".into(),
            tools: "read×2 · edit".into(),
            reason: "completed".into(),
            steps: 5,
            reply_count: 2,
            texts: vec!["改好了，测试通过。".into()],
            ends_with_question: false,
            ..Default::default()
        }
    }

    #[test]
    fn signals_line_reports_what_the_program_knows() {
        let mut f = base();
        f.failures = vec!["bash: exit 1".into()];
        f.ends_with_question = true;
        let s = f.signals();
        assert!(s.contains("结尾是问句=是"), "{s}");
        assert!(s.contains("失败调用=1"), "{s}");
        assert!(s.contains("用过提问工具=否"), "{s}");
    }

    #[test]
    fn gate_answers_error_and_suspension_without_a_model() {
        let mut f = base();
        assert_eq!(gate(&f), None);
        f.reason = "error".into();
        assert_eq!(gate(&f), Some(State::Stalled));

        let mut f = base();
        f.subagents = vec!["还在跑".into()];
        assert_eq!(gate(&f), Some(State::Waiting));
    }

    #[test]
    fn parse_survives_thinking_wrappers_and_prose() {
        assert_eq!(parse_state(r#"{"state":"done"}"#), Some(State::Done));
        assert_eq!(
            parse_state(" thinking\n\n<｜end▁of▁thinking｜>\n\n{\"state\":\"needs_user\"}"),
            Some(State::NeedsUser)
        );
        assert_eq!(parse_state("stalled"), Some(State::Stalled));
        assert_eq!(parse_state("完全没有"), None);
    }

    #[tokio::test]
    async fn rule_judge_is_conservative() {
        let j = RuleJudge;
        assert_eq!(j.judge(&base()).await, Some(State::Done));

        let mut q = base();
        q.ends_with_question = true;
        assert_eq!(j.judge(&q).await, Some(State::NeedsUser));

        let mut silent = base();
        silent.texts.clear();
        assert_eq!(j.judge(&silent).await, Some(State::Stalled));
    }

    #[tokio::test]
    async fn service_prefers_the_gate() {
        let svc = LayoutService::new(Arc::new(RuleJudge));
        let mut f = base();
        f.reason = "aborted".into();
        let out = svc.judge_turn(Some(3), &f).await;
        assert_eq!(out.state, State::Stalled);
        assert_eq!(out.source, Source::Gate);
        assert_eq!(out.turn, Some(3));
        assert_eq!(out.v, Layout::VERSION);
    }
}

#[cfg(test)]
mod assembly_tests {
    use super::*;

    #[test]
    fn facts_follow_the_training_rules() {
        let items = vec![
            Item::User("把 X 改掉"),
            Item::Step,
            Item::Assistant("我先看一下。"),
            Item::Tool("read"),
            Item::Tool("read"),
            Item::ToolFailed("bash: exit 1"),
            Item::Step,
            Item::Assistant("改好了，要我提交吗？"),
            Item::TurnEnd("completed"),
        ];
        let f = facts_from_turn("s:7", 2.5, items);
        assert_eq!(f.user, "把 X 改掉");
        assert_eq!(f.tools, "read×2");
        assert_eq!(f.failures.len(), 1);
        assert_eq!(f.steps, 2);
        assert_eq!(f.reply_count, 2);
        assert_eq!(f.reason, "completed");
        assert!(f.ends_with_question);
        assert_eq!(f.texts.len(), 2);
    }

    #[test]
    fn only_the_last_three_messages_are_kept() {
        let items = vec![
            Item::User("u"),
            Item::Assistant("a1"),
            Item::Assistant("a2"),
            Item::Assistant("a3"),
            Item::Assistant("a4"),
        ];
        let f = facts_from_turn("s:1", 1.0, items);
        assert_eq!(f.reply_count, 4);
        assert_eq!(f.texts, vec!["a2", "a3", "a4"]);
    }
}

#[cfg(test)]
mod state_machine_tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            user: "跑一下测试".into(),
            reason: "completed".into(),
            texts: vec!["都通过了。".into()],
            ..Facts::default()
        }
    }

    #[test]
    fn structure_beats_the_model() {
        let mut f = facts();
        f.reason = "error".into();
        assert_eq!(state_of_turn(&f, Some(State::Done)), SessionState::Failed);

        let mut f = facts();
        f.reason = "aborted".into();
        assert_eq!(state_of_turn(&f, Some(State::Done)), SessionState::Paused);

        let mut f = facts();
        f.subagents = vec!["还在跑".into()];
        assert_eq!(state_of_turn(&f, Some(State::Done)), SessionState::Subagent);

        let mut f = facts();
        f.jobs = vec!["build".into()];
        assert_eq!(state_of_turn(&f, Some(State::Done)), SessionState::Background);
    }

    #[test]
    fn the_model_decides_the_semantic_case() {
        assert_eq!(state_of_turn(&facts(), Some(State::Done)), SessionState::Completed);
        assert_eq!(
            state_of_turn(&facts(), Some(State::NeedsUser)),
            SessionState::AwaitingUser
        );
        assert_eq!(state_of_turn(&facts(), Some(State::Stalled)), SessionState::Failed);
    }

    #[test]
    fn without_a_model_the_rules_are_conservative() {
        assert_eq!(state_of_turn(&facts(), None), SessionState::Completed);

        let mut q = facts();
        q.ends_with_question = true;
        assert_eq!(state_of_turn(&q, None), SessionState::AwaitingUser);

        let mut silent = facts();
        silent.texts.clear();
        assert_eq!(state_of_turn(&silent, None), SessionState::Failed);
    }

    #[test]
    fn live_signals_map_one_to_one() {
        assert_eq!(Live::Idle.state(), SessionState::Idle);
        assert_eq!(Live::Thinking.state(), SessionState::Thinking);
        assert_eq!(Live::Writing.state(), SessionState::StreamNarration);
        assert_eq!(Live::Tool.state(), SessionState::Tool);
    }
}
