//! The skill table the phone's command palette draws.
//!
//! A skill is a directory with a `SKILL.md` in it, installed per agent under
//! `<home>/skills/<name>/SKILL.md`. This module turns that much disk into the
//! delta one `skill.catalog` call answers with.
//!
//! Four decisions are worth knowing before reading the code:
//!
//! - **The fingerprint is the text, not the file.** FNV-1a over
//!   `name + U+001F + description` means `touch` and an editor's
//!   save-without-changes both leave the phone's cache alone. Anything derived
//!   from mtime turns a routine backup into a thousand bogus updates.
//! - **A broken `SKILL.md` is still a skill.** Frontmatter is a YAML subset we
//!   read by hand; when it fails, the directory name is the skill and the
//!   description is empty. One bad file must not cost the phone the other 900.
//! - **An incomplete scan may not delete.** `complete` is false when an engine
//!   directory could not be read or when the cap was hit, and then `removed` is
//!   empty: "I did not see it" and "it is gone" would otherwise be the same
//!   answer, and the phone would drop rows that are still installed.
//! - **The cache is 30 seconds, not the process lifetime.** A skill installed
//!   in the desktop app shows up on the next pull without a restart; a phone
//!   that refreshes twice in a row does not make this machine read the disk
//!   twice.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agi_protocol::engine::EngineId;
use agi_protocol::skills::{
    KnownSkill, SkillCatalogRequest, SkillCatalogResponse, SkillEngineStatus, SkillEntry,
};

/// The file that makes a directory a skill.
const SKILL_FILE: &str = "SKILL.md";

/// A `SKILL.md` bigger than this is a mistake, not a skill.
const MAX_SKILL_BYTES: u64 = 256 * 1024;

/// How many skills the host sends when the phone names no ceiling.
pub const DEFAULT_MAX_SKILLS: usize = 1000;

/// The hard ceiling, whatever the phone asked for.
pub const MAX_SKILLS: usize = 2000;

/// How much of a description survives.
const MAX_DESCRIPTION_CHARS: usize = 600;

/// The largest `skill.catalog` answer, before it is trimmed from the tail.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// How long one scan is reused.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// How many different scan sets are kept. A host has three engines and a test
/// has a few temporary directories; more than that is a leak.
const CACHE_ENTRIES: usize = 4;

/// One pass over the disk, before anyone said what the phone already holds.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// Every skill, deduped in `[Dsh, Claude, Codex]` order and sorted by id.
    pub skills: Vec<SkillEntry>,
    /// What each engine this scan looked at contributed, in scan order.
    pub engines: Vec<SkillEngineStatus>,
    /// Whether the cap cut the list short.
    pub truncated: bool,
}

impl Discovery {
    /// Whether this pass saw everything there is.
    ///
    /// The only state in which the phone may drop rows: both a failed engine
    /// and a capped list are "not all of it", which is why they collapse into
    /// one bit.
    pub fn complete(&self) -> bool {
        !self.truncated && self.engines.iter().all(|engine| engine.error.is_none())
    }
}

/// Read every skill under the given engine homes, without the cache.
///
/// `roots` pairs each agent with the directory its `skills/` lives under — the
/// same `dsh_home` / `claude_home` / `codex_home` the rest of the app uses. The
/// order is the dedupe order: the first engine to offer an id wins, because a
/// skill installed for all three is one row in the palette, not three.
pub fn discover(roots: &[(EngineId, PathBuf)], max_skills: usize) -> Discovery {
    let max = max_skills.clamp(1, MAX_SKILLS);
    let mut engines = Vec::with_capacity(roots.len());
    let mut skills: Vec<SkillEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (engine, home) in roots {
        let scan = scan_engine(home);
        engines.push(SkillEngineStatus {
            id: *engine,
            count: scan.skills.len() as u64,
            error: scan.error,
        });
        for scanned in scan.skills {
            if !seen.insert(scanned.name.clone()) {
                continue;
            }
            skills.push(SkillEntry {
                fingerprint: fingerprint(&scanned.name, &scanned.description),
                engine: *engine,
                id: scanned.name.clone(),
                name: scanned.name,
                description: scanned.description,
            });
        }
    }

    // Sorted by id so two hosts -- and two runs of the same host -- answer in
    // the same order. Everything above depends on the order engines were tried;
    // nothing below does.
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    let truncated = skills.len() > max;
    skills.truncate(max);

    Discovery {
        skills,
        engines,
        truncated,
    }
}

/// Answer one `skill.catalog` request, reusing a scan younger than 30 seconds.
pub fn catalog(
    roots: &[(EngineId, PathBuf)],
    request: &SkillCatalogRequest,
) -> SkillCatalogResponse {
    let cached = cached(roots, clamp_max_skills(request.max_skills));
    answer(&cached.discovery, &request.known, cached.scanned_at)
}

/// The host's ceiling, whatever the phone asked for.
///
/// Clamped at the bottom too: `maxSkills: 0` from a broken client would answer
/// "you have no skills" with `complete: false`, and a phone that never gets an
/// entry can never be right again.
pub fn clamp_max_skills(requested: Option<u32>) -> usize {
    requested
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_MAX_SKILLS)
        .clamp(1, MAX_SKILLS)
}

/// The answer for one discovery: the delta, the deletions, and the frame budget.
fn answer(discovery: &Discovery, known: &[KnownSkill], scanned_at: u64) -> SkillCatalogResponse {
    let held: HashMap<&str, &str> = known
        .iter()
        .map(|entry| (entry.id.as_str(), entry.fingerprint.as_str()))
        .collect();

    let skills: Vec<SkillEntry> = discovery
        .skills
        .iter()
        .filter(|skill| {
            !held
                .get(skill.id.as_str())
                .is_some_and(|fingerprint| *fingerprint == skill.fingerprint.as_str())
        })
        .cloned()
        .collect();

    let complete = discovery.complete();
    let mut removed = Vec::new();
    if complete {
        removed = known
            .iter()
            .filter(|entry| !discovery.skills.iter().any(|skill| skill.id == entry.id))
            .map(|entry| entry.id.clone())
            .collect();
        // Sorted and deduped: the phone's cache is a dictionary, but an answer
        // that depends on the order of the phone's own array is untestable.
        removed.sort();
        removed.dedup();
    }

    let mut response = SkillCatalogResponse {
        skills,
        removed,
        complete,
        scanned_at,
        engines: discovery.engines.clone(),
        truncated: discovery.truncated,
    };
    trim_to_frame(&mut response);
    response
}

/// Cut the answer down to a frame a phone can take.
///
/// The design's budget is 512 KiB of serialized answer. Dropping entries from
/// the tail is safe because the list is sorted and the phone merges a delta
/// into its cache rather than replacing it — but the answer must then stop
/// claiming completeness, and an incomplete answer may not name deletions.
fn trim_to_frame(response: &mut SkillCatalogResponse) {
    if serialized_len(response) <= MAX_RESPONSE_BYTES {
        return;
    }
    response.complete = false;
    response.truncated = true;
    response.removed.clear();

    // The fixed cost, measured with the list emptied, so the per-entry
    // accounting below only has to add the entries themselves.
    let mut empty = response.clone();
    empty.skills.clear();
    let mut used = serialized_len(&empty);
    let mut keep = 0;
    for skill in &response.skills {
        // Plus the comma before it. One byte over budget is a byte the frame
        // does not have.
        let size = serialized_len(skill) + 1;
        if used + size > MAX_RESPONSE_BYTES {
            break;
        }
        used += size;
        keep += 1;
    }
    response.skills.truncate(keep);
}

/// One scan and the two clocks it is known by.
#[derive(Clone)]
struct Cached {
    roots: Vec<(EngineId, PathBuf)>,
    max_skills: usize,
    at: Instant,
    scanned_at: u64,
    discovery: Discovery,
}

/// The 30-second scan cache.
///
/// Keyed on the roots, not on "the machine": a test with three temporary homes
/// and a real host in the same process must not answer for each other.
static CACHE: Mutex<Vec<Cached>> = Mutex::new(Vec::new());

fn cache_lock() -> MutexGuard<'static, Vec<Cached>> {
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cached(roots: &[(EngineId, PathBuf)], max_skills: usize) -> Cached {
    let mut cache = cache_lock();
    cache.retain(|entry| entry.at.elapsed() < CACHE_TTL);
    if let Some(position) = cache
        .iter()
        .position(|entry| entry.roots == roots && entry.max_skills == max_skills)
    {
        return cache[position].clone();
    }
    drop(cache);

    let discovery = discover(roots, max_skills);
    let fresh = Cached {
        roots: roots.to_vec(),
        max_skills,
        at: Instant::now(),
        scanned_at: now_ms(),
        discovery,
    };
    let mut cache = cache_lock();
    while cache.len() >= CACHE_ENTRIES {
        cache.remove(0);
    }
    cache.push(fresh.clone());
    fresh
}

/// What one engine's directory held.
struct EngineScan {
    skills: Vec<Scanned>,
    error: Option<String>,
}

/// One skill, as the disk named it.
struct Scanned {
    name: String,
    description: String,
}

/// Read one engine's `skills/` directory.
fn scan_engine(home: &Path) -> EngineScan {
    let directory = home.join("skills");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        // An agent with no skills directory is the normal case on a fresh
        // machine, and is not a failure to report.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return EngineScan {
                skills: Vec::new(),
                error: None,
            }
        }
        Err(error) => {
            return EngineScan {
                skills: Vec::new(),
                error: Some(format!("{}: {error}", directory.display())),
            }
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                // A name that is not UTF-8 cannot be a skill id, and there is
                // nothing useful to say about it.
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
            Err(error) => {
                return EngineScan {
                    skills: Vec::new(),
                    error: Some(format!("{}: {error}", directory.display())),
                }
            }
        }
    }
    // Directory order is the filesystem's, which differs between machines and
    // between runs on the same machine. Sorting is what makes the answer
    // reproducible, including which duplicate wins.
    names.sort();

    let mut skills = Vec::new();
    let mut error: Option<String> = None;
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        match read_skill(&directory.join(&name), &name) {
            Ok(Some(skill)) => skills.push(skill),
            Ok(None) => {}
            Err(message) => {
                // Keep what we could read, but say so: a partial answer must
                // not be read as "the rest of your skills are gone".
                if error.is_none() {
                    error = Some(message);
                }
            }
        }
    }
    EngineScan { skills, error }
}

/// Read one `<name>/SKILL.md`, or say why not.
///
/// `Ok(None)` is "this is not a skill" — a plain file, a directory with no
/// `SKILL.md`, or one too big to be one. `Err` is "there is a skill here and
/// this machine could not read it", which is the only case worth reporting.
fn read_skill(directory: &Path, name: &str) -> Result<Option<Scanned>, String> {
    let metadata = match std::fs::metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", directory.display())),
    };
    if !metadata.is_dir() {
        return Ok(None);
    }

    let file = directory.join(SKILL_FILE);
    let metadata = match std::fs::metadata(&file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", file.display())),
    };
    if !metadata.is_file() || metadata.len() > MAX_SKILL_BYTES {
        return Ok(None);
    }

    match std::fs::read_to_string(&file) {
        Ok(text) if text.len() as u64 <= MAX_SKILL_BYTES => {
            let (name, description) = parse_frontmatter(&text, name);
            Ok(Some(Scanned { name, description }))
        }
        // Too big in bytes after all, or not UTF-8: a skill whose metadata we
        // cannot read is still installed.
        Ok(_) => Ok(bare(name)),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(bare(name)),
        Err(error) => Err(format!("{}: {error}", file.display())),
    }
}

/// A skill we know is installed but could not describe.
fn bare(name: &str) -> Option<Scanned> {
    Some(Scanned {
        name: name.to_string(),
        description: String::new(),
    })
}

/// The `name` and `description` a `SKILL.md` declares.
///
/// The frontmatter is a YAML subset, read by hand: the first `---` to the next
/// one, top-level `name` and `description` only, with plain, quoted and block
/// scalars. Everything is best-effort — a file we cannot parse still produces a
/// row, named after its directory.
fn parse_frontmatter(text: &str, directory: &str) -> (String, String) {
    let Some(body) = frontmatter_body(text) else {
        return (directory.to_string(), String::new());
    };

    let mut name = None;
    let mut description = None;
    let mut index = 0;
    while index < body.len() {
        let line = body[index];
        index += 1;
        let Some((key, inline)) = split_key(line) else {
            // An indented line belongs to the key above it; a stray column-zero
            // line that is not `key: value` is not YAML at all, but losing the
            // whole file over it would be worse than skipping it.
            continue;
        };
        if key != "name" && key != "description" {
            // Its block, if it has one, is indented and skipped by the same
            // rule as above.
            continue;
        }
        let (value, consumed) = read_value(&body[index..], inline);
        index += consumed;
        if let Some(value) = value {
            if key == "name" {
                name = Some(value);
            } else {
                description = Some(value);
            }
        }
    }

    let name = name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| directory.to_string());
    let description = collapse_whitespace(&description.unwrap_or_default());
    (name, truncate_chars(&description, MAX_DESCRIPTION_CHARS))
}

/// The lines between the opening and closing `---`, if the file has both.
fn frontmatter_body(text: &str) -> Option<Vec<&str>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            continue;
        }
        if line.trim_end() != "---" {
            return None;
        }
        break;
    }
    let mut body = Vec::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(body);
        }
        body.push(line);
    }
    // An opening `---` with no closing one is not frontmatter; treating the
    // whole file as a description would put a README in the palette.
    None
}

/// `key: value`, for a line at column zero.
fn split_key(line: &str) -> Option<(&str, &str)> {
    if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some((key, value.trim()))
}

/// One value, and how many following lines it used up.
fn read_value(rest: &[&str], inline: &str) -> (Option<String>, usize) {
    let Some(folded) = block_indicator(inline) else {
        return (parse_scalar(inline), 0);
    };
    let mut block = Vec::new();
    let mut consumed = 0;
    for line in rest {
        // A block scalar runs to the next column-zero line; blank lines belong
        // to it, which is how YAML writes a paragraph break.
        if !line.trim().is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            break;
        }
        block.push(*line);
        consumed += 1;
    }
    (Some(unindent(&block, folded)), consumed)
}

/// Whether a value is a block scalar, and whether it folds.
///
/// `|` keeps the lines, `>` folds them, and the chomping indicator (`-`/`+`)
/// after either one decides what happens to the trailing newline — which this
/// module then throws away, because a description is one line by contract.
fn block_indicator(inline: &str) -> Option<bool> {
    let indicator = inline.chars().next()?;
    if indicator != '|' && indicator != '>' {
        return None;
    }
    let tail = &inline[indicator.len_utf8()..];
    let tail = tail.split('#').next().unwrap_or(tail);
    let plain = tail
        .chars()
        .all(|c| c == '-' || c == '+' || c.is_ascii_digit() || c.is_whitespace());
    plain.then_some(indicator == '>')
}

/// Strip the block's common indentation and join the lines.
fn unindent(lines: &[&str], folded: bool) -> String {
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);

    let mut out = String::new();
    for line in lines {
        let line = if line.trim().is_empty() {
            ""
        } else {
            line.get(indent..).unwrap_or(line).trim_end()
        };
        if line.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(if folded { ' ' } else { '\n' });
        }
        out.push_str(line);
    }
    out
}

/// One scalar: plain, single-quoted or double-quoted.
fn parse_scalar(inline: &str) -> Option<String> {
    let inline = inline.trim();
    if let Some(rest) = inline.strip_prefix('"') {
        return unescape_double(rest);
    }
    if let Some(rest) = inline.strip_prefix('\'') {
        return unescape_single(rest);
    }
    Some(inline.to_string())
}

/// A double-quoted scalar, minus its closing quote.
///
/// `None` is an unterminated string: the value is unusable, and the caller
/// falls back rather than half-reading it.
fn unescape_double(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        match character {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

/// A single-quoted scalar, where `''` is the escape for a quote.
fn unescape_single(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\'' {
            out.push(character);
            continue;
        }
        if chars.peek() == Some(&'\'') {
            chars.next();
            out.push('\'');
            continue;
        }
        return Some(out);
    }
    None
}

/// One line, however the file wrapped it.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut to `limit` characters — characters, not bytes.
///
/// A byte slice through a Chinese description ends mid-character and the
/// JSON writer then has to decide what to do with it; counting characters is
/// what the design promises the phone.
fn truncate_chars(text: &str, limit: usize) -> String {
    let mut cut: String = text.chars().take(limit).collect();
    // The cut can land before a space, and a trailing space is not a
    // description.
    let trimmed = cut.trim_end().len();
    cut.truncate(trimmed);
    cut
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

fn serialized_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(usize::MAX)
}

/// FNV-1a 64 of the text that matters, as 16 lowercase hex digits.
///
/// Hand-written rather than a dependency: it is nine lines, and the value only
/// ever has to be stable between the host that writes it and the phone that
/// compares it.
pub fn fingerprint(name: &str, description: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in [name.as_bytes(), b"\x1f", description.as_bytes()] {
        for byte in part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(home: &Path, engine: EngineId, name: &str, body: &str) -> PathBuf {
        let directory = home.join(engine.as_str()).join("skills").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(SKILL_FILE), body).unwrap();
        directory
    }

    fn roots(home: &Path) -> Vec<(EngineId, PathBuf)> {
        EngineId::all()
            .into_iter()
            .map(|engine| (engine, home.join(engine.as_str())))
            .collect()
    }

    fn discover_all(home: &Path) -> Discovery {
        discover(&roots(home), DEFAULT_MAX_SKILLS)
    }

    fn known(entries: &[(&str, &str)]) -> Vec<KnownSkill> {
        entries
            .iter()
            .map(|(id, fingerprint)| KnownSkill {
                id: (*id).to_string(),
                fingerprint: (*fingerprint).to_string(),
            })
            .collect()
    }

    /// A `SKILL.md` with both keys, for the tests that need them to agree.
    fn named(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n")
    }

    /// A skill body with one description, for the tests that only care about it.
    fn body(description: &str) -> String {
        named("skill", description)
    }

    #[test]
    fn frontmatter_reads_plain_keys() {
        let text = "---\nname: grilling\ndescription: Grill the user about a plan.\n---\n\nBody\n";
        let (name, description) = parse_frontmatter(text, "directory");
        assert_eq!(name, "grilling");
        assert_eq!(description, "Grill the user about a plan.");
    }

    #[test]
    fn frontmatter_unquotes_both_styles() {
        let text = "---\nname: \"grilling\"\ndescription: 'It''s a \"plan\" test'\n---\n";
        let (name, description) = parse_frontmatter(text, "directory");
        assert_eq!(name, "grilling");
        assert_eq!(description, "It's a \"plan\" test");

        // A double-quoted value keeps its escapes meaningful, and the
        // whitespace collapse then flattens the newline.
        let escaped = "---\nname: folded\ndescription: \"first\\nsecond\"\n---\n";
        assert_eq!(parse_frontmatter(escaped, "directory").1, "first second");
    }

    #[test]
    fn frontmatter_folds_a_greater_than_block() {
        let text = "---\nname: folded\ndescription: >-\n  Grill the user relentlessly\n  about a plan, then\n  stop.\n---\n";
        let (name, description) = parse_frontmatter(text, "directory");
        assert_eq!(name, "folded");
        assert_eq!(
            description,
            "Grill the user relentlessly about a plan, then stop."
        );
    }

    #[test]
    fn frontmatter_reads_a_literal_block_as_one_line() {
        // `|` keeps the paragraph break; a description is one line by contract,
        // so the break becomes a space exactly like a fold would.
        let text = "---\nname: block\ndescription: |\n  First line.\n\n  Second line.\n---\n";
        let (name, description) = parse_frontmatter(text, "directory");
        assert_eq!(name, "block");
        assert_eq!(description, "First line. Second line.");
    }

    #[test]
    fn frontmatter_without_a_block_names_the_skill_after_its_directory() {
        for text in [
            "Just a body, no frontmatter.\n",
            "---\nname: never closed\n",
            "",
        ] {
            let (name, description) = parse_frontmatter(text, "handoff");
            assert_eq!(name, "handoff", "text was {text:?}");
            assert_eq!(description, "", "text was {text:?}");
        }
    }

    #[test]
    fn broken_frontmatter_falls_back_instead_of_failing() {
        // A broken line costs that line's value, not the whole skill.
        let broken_quote = "---\nname: \"grilling\ndescription: fine\n---\n";
        let (name, description) = parse_frontmatter(broken_quote, "directory-name");
        assert_eq!(name, "directory-name");
        assert_eq!(description, "fine");

        // An empty value is not a name.
        let empty = "---\nname:\ndescription: fine\n---\n";
        assert_eq!(parse_frontmatter(empty, "fallback").0, "fallback");

        // A block scalar that never ends is still readable.
        let unclosed = "---\nname: grilling\ndescription: >-\n  never closed\n";
        assert_eq!(
            parse_frontmatter(unclosed, "fallback"),
            ("fallback".to_string(), String::new())
        );
    }

    #[test]
    fn other_keys_and_their_children_are_ignored() {
        let text = "---\nname: grilling\ndescription: >-\n  Grill it.\nmetadata:\n  short-description: x\nallowed-tools: Bash Read\n---\n";
        let (name, description) = parse_frontmatter(text, "directory");
        assert_eq!(name, "grilling");
        assert_eq!(description, "Grill it.");
    }

    #[test]
    fn discover_dedupes_in_engine_order_and_sorts_by_id() {
        let temp = tempfile::tempdir().unwrap();
        for engine in EngineId::all() {
            install(
                temp.path(),
                engine,
                "shared",
                &named("shared", &format!("from {engine}")),
            );
        }
        install(
            temp.path(),
            EngineId::Claude,
            "only-claude",
            &named("only-claude", "claude"),
        );

        let discovery = discover_all(temp.path());
        assert!(discovery.complete());
        let ids: Vec<&str> = discovery
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect();
        assert_eq!(ids, ["only-claude", "shared"]);

        // The first engine to offer the id wins, whole: no merged description,
        // no second row.
        let shared = discovery
            .skills
            .iter()
            .find(|skill| skill.id == "shared")
            .unwrap();
        assert_eq!(shared.engine, EngineId::Dsh);
        assert_eq!(shared.description, "from dsh");
        assert_eq!(shared.name, "shared");

        // `count` is what each engine held before dedupe.
        let counts: Vec<u64> = discovery
            .engines
            .iter()
            .map(|engine| engine.count)
            .collect();
        assert_eq!(counts, [1, 2, 1]);
    }

    #[test]
    fn discover_skips_what_is_not_a_skill() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(EngineId::Dsh.as_str());
        // A dot directory, a directory with no SKILL.md, a loose file, and a
        // SKILL.md past the size ceiling are all not skills.
        std::fs::create_dir_all(home.join("skills").join(".hidden")).unwrap();
        std::fs::write(home.join("skills").join(".hidden").join(SKILL_FILE), "x").unwrap();
        std::fs::create_dir_all(home.join("skills").join("empty")).unwrap();
        std::fs::write(home.join("skills").join("loose.md"), "x").unwrap();
        install(
            temp.path(),
            EngineId::Dsh,
            "huge",
            &named("huge", &"x".repeat(MAX_SKILL_BYTES as usize)),
        );
        install(temp.path(), EngineId::Dsh, "good", &named("good", "ok"));

        let discovery = discover_all(temp.path());
        assert!(discovery.complete());
        let ids: Vec<&str> = discovery
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect();
        assert_eq!(ids, ["good"]);
    }

    #[test]
    fn descriptions_are_collapsed_and_cut_on_characters() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            EngineId::Dsh,
            "spaced",
            "---\nname: spaced\ndescription: |\n  one\n\n  two\t\tthree   four\n---\n",
        );
        let long = "汉".repeat(700);
        install(temp.path(), EngineId::Dsh, "long", &named("long", &long));

        let discovery = discover_all(temp.path());
        let spaced = discovery
            .skills
            .iter()
            .find(|skill| skill.id == "spaced")
            .unwrap();
        assert_eq!(spaced.description, "one two three four");

        let long = discovery
            .skills
            .iter()
            .find(|skill| skill.id == "long")
            .unwrap();
        // 600 characters of a 2100-byte description: a byte cut would have
        // landed inside one.
        assert_eq!(long.description.chars().count(), MAX_DESCRIPTION_CHARS);
        assert_eq!(long.description.len(), MAX_DESCRIPTION_CHARS * 3);
    }

    #[test]
    fn the_fingerprint_follows_the_text_and_not_the_clock() {
        let temp = tempfile::tempdir().unwrap();
        let directory = install(temp.path(), EngineId::Dsh, "grilling", &body("first"));
        let before = discover_all(temp.path()).skills[0].fingerprint.clone();
        assert_eq!(before.len(), 16);
        assert!(before
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));

        // A touched file is not a changed skill.
        let file = std::fs::File::options()
            .write(true)
            .open(directory.join(SKILL_FILE))
            .unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        drop(file);
        assert_eq!(discover_all(temp.path()).skills[0].fingerprint, before);

        // Rewriting the description is.
        std::fs::write(directory.join(SKILL_FILE), body("second")).unwrap();
        let after = discover_all(temp.path()).skills[0].fingerprint.clone();
        assert_ne!(after, before);
        // And it is a function of the text alone, so the same text hashes the
        // same after a round trip through another directory.
        assert_eq!(after, fingerprint("skill", "second"));
    }

    #[test]
    fn a_delta_carries_only_what_changed() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            EngineId::Dsh,
            "alpha",
            "---\nname: alpha\ndescription: a\n---\n",
        );
        install(
            temp.path(),
            EngineId::Dsh,
            "beta",
            "---\nname: beta\ndescription: b\n---\n",
        );
        let discovery = discover_all(temp.path());
        let alpha = discovery.skills[0].fingerprint.clone();
        let beta = discovery.skills[1].fingerprint.clone();

        // One unchanged, one new: only the new one is sent.
        let delta = answer(&discovery, &known(&[("alpha", alpha.as_str())]), 7);
        let ids: Vec<&str> = delta.skills.iter().map(|skill| skill.id.as_str()).collect();
        assert_eq!(ids, ["beta"]);
        assert!(delta.complete);
        assert_eq!(delta.scanned_at, 7);
        assert!(delta.removed.is_empty());

        // Everything known and matching is an empty delta, not a failure.
        let quiet = answer(
            &discovery,
            &known(&[("alpha", alpha.as_str()), ("beta", beta.as_str())]),
            7,
        );
        assert!(quiet.skills.is_empty());
        assert!(quiet.complete);

        // A fingerprint that no longer matches comes back.
        let all = answer(&discovery, &known(&[("alpha", "0000000000000000")]), 7);
        assert_eq!(all.skills.len(), 2);
    }

    #[test]
    fn removed_names_only_what_the_phone_holds_and_the_disk_does_not() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            EngineId::Dsh,
            "alpha",
            "---\nname: alpha\ndescription: a\n---\n",
        );
        let discovery = discover_all(temp.path());

        let response = answer(
            &discovery,
            &known(&[("alpha", "stale"), ("gone", "whatever")]),
            1,
        );
        assert_eq!(response.removed, ["gone"]);
        assert!(response.complete);

        // A capped scan may not claim deletions.
        let mut capped = discovery.clone();
        capped.truncated = true;
        let response = answer(&capped, &known(&[("gone", "x")]), 1);
        assert!(response.removed.is_empty());
        assert!(!response.complete);
        assert!(response.truncated);

        // Neither may an engine that failed to read.
        let mut broken = discovery.clone();
        broken.engines[0].error = Some("permission denied".to_string());
        assert!(answer(&broken, &known(&[("gone", "x")]), 1)
            .removed
            .is_empty());
        assert!(!broken.complete());
    }

    #[test]
    fn max_skills_is_a_cap_and_says_so() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            install(temp.path(), EngineId::Dsh, name, "plain body");
        }
        let discovery = discover(&roots(temp.path()), 2);
        assert_eq!(discovery.skills.len(), 2);
        assert!(discovery.truncated);
        assert!(!discovery.complete());

        // The ceiling is clamped in both directions, so a hostile `maxSkills`
        // cannot turn the palette empty for ever.
        assert_eq!(clamp_max_skills(Some(0)), 1);
        assert_eq!(clamp_max_skills(Some(99_999)), MAX_SKILLS);
        assert_eq!(clamp_max_skills(None), DEFAULT_MAX_SKILLS);
    }

    #[test]
    fn an_answer_over_the_frame_budget_is_trimmed_from_the_tail() {
        let entry = |index: usize| SkillEntry {
            id: format!("skill-{index:04}"),
            name: format!("skill-{index:04}"),
            description: "x".repeat(MAX_DESCRIPTION_CHARS),
            fingerprint: "0".repeat(16),
            engine: EngineId::Dsh,
        };
        let mut small = SkillCatalogResponse {
            skills: vec![entry(0)],
            removed: vec!["gone".to_string()],
            complete: true,
            scanned_at: 1,
            engines: vec![SkillEngineStatus {
                id: EngineId::Dsh,
                count: 1,
                error: None,
            }],
            truncated: false,
        };
        trim_to_frame(&mut small);
        assert_eq!(small.skills.len(), 1, "a small answer is untouched");
        assert!(small.complete);

        let mut big = SkillCatalogResponse {
            skills: (0..900).map(entry).collect(),
            removed: vec!["gone".to_string()],
            ..small
        };
        trim_to_frame(&mut big);
        assert!(big.skills.len() < 900);
        assert!(big.skills.len() > 600, "only the overflow goes");
        assert!(big.truncated);
        assert!(!big.complete);
        assert!(
            big.removed.is_empty(),
            "an incomplete answer may not delete"
        );
        assert_eq!(big.skills[0].id, "skill-0000", "the tail goes, in order");
        assert!(serde_json::to_string(&big).unwrap().len() <= MAX_RESPONSE_BYTES);
    }

    #[test]
    fn a_catalog_call_answers_the_whole_table_when_the_phone_holds_nothing() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), EngineId::Dsh, "alpha", &body("a"));

        let response = catalog(&roots(temp.path()), &SkillCatalogRequest::default());
        assert!(response.complete);
        assert!(!response.truncated);
        assert_eq!(response.skills.len(), 1);
        assert_eq!(response.engines.len(), 3);
        assert!(
            response.scanned_at > 1_577_836_800_000,
            "scannedAt is Unix milliseconds"
        );

        // The second call is served from the 30-second cache, and the cached
        // scan is the same table.
        let again = catalog(&roots(temp.path()), &SkillCatalogRequest::default());
        assert_eq!(again.skills, response.skills);
        assert_eq!(again.engines, response.engines);
    }
}
