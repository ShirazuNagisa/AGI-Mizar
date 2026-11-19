//! Read-only Git status for one session's workspace.
//!
//! The phone's composer draws a pill with "how much is uncommitted here", which
//! is the only question this module answers. Nothing else on the host runs Git:
//! the sidebar never asks, and a session that is not on screen is never scanned.
//!
//! The commands are read-only by construction — `git diff --numstat` against
//! `HEAD` plus the untracked list — and run without a shell, without a
//! repository's own diff driver, fsmonitor hook or clean filter, and without
//! optional locks. A phone's refresh must not execute a repository's
//! configuration, write the index, leave `index.lock` behind, read through an
//! untracked symlink, fetch a missing object, or descend into a submodule.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};

/// How long one Git invocation may take before it is killed.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The tree a repository with no commit diffs against, so "nothing committed
/// yet" counts the staged content as additions instead of answering nothing.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The most of one untracked file that is read to count its lines.
const UNTRACKED_FILE_MAX: u64 = 4 * 1024 * 1024;

/// The most, across all untracked files, that one answer reads.
const UNTRACKED_TOTAL_MAX: u64 = 32 * 1024 * 1024;

/// What the composer's Git pill draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitSummary {
    pub files: u64,
    pub additions: u64,
    pub deletions: u64,
}

impl GitSummary {
    /// The wire shape: three counts, never a path.
    pub fn to_json(self) -> Value {
        json!({
            "files": self.files,
            "additions": self.additions,
            "deletions": self.deletions,
        })
    }
}

/// The uncommitted change in the repository that holds `cwd`.
///
/// `Ok(None)` means there is no repository to draw here (or no trustworthy
/// directory to ask about). `Err` means Git ran and failed, which the phone must
/// keep apart from a clean tree: it keeps what it already shows instead of
/// clearing it.
pub async fn summarize(cwd: &str) -> Result<Option<GitSummary>, String> {
    let Some(dir) = trusted_dir(cwd).await else {
        return Ok(None);
    };
    let Some(root) = discover(&dir).await? else {
        return Ok(None);
    };

    let head = run(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).await?;
    let base = if head.success {
        // Let Git name the commit: a hash this side picked could disagree with
        // the repository's own idea of `HEAD`.
        "HEAD"
    } else if head.code == Some(1) {
        // No commits yet: every staged file is an addition against the empty
        // tree, which is what `git status` reports for a fresh repository.
        EMPTY_TREE
    } else {
        return Err(failed("rev-parse HEAD", &root, &head));
    };

    // `--no-ext-diff` and `--no-textconv` do not cover the clean filter: reading
    // the working tree runs `filter.<driver>.clean` (or `.process`) named by a
    // `.gitattributes` rule. Override every driver the effective configuration
    // defines, using the same command-scope environment the diff will read.
    let filters = filter_overrides(&root).await?;
    let diff = run_with(
        &root,
        &[
            "diff",
            "--numstat",
            "-z",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            // A submodule is shown as the commit the superproject records, so
            // the diff never walks into the submodule's own work tree.
            "--submodule=short",
            "--ignore-submodules=dirty",
            base,
        ],
        &filters,
    )
    .await?;
    if !diff.success {
        return Err(failed("diff", &root, &diff));
    }
    let (files, additions, deletions) = parse_numstat(&diff.stdout);

    let others = run(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    if !others.success {
        return Err(failed("ls-files", &root, &others));
    }
    let untracked: Vec<PathBuf> = split_nul(&others.stdout).map(path_from_bytes).collect();

    // Reading untracked files is file IO, and a repository can hold thousands of
    // them: keep it off the runtime's threads.
    let (untracked_files, untracked_lines) =
        tokio::task::spawn_blocking(move || count_untracked(&root, &untracked))
            .await
            .map_err(|error| format!("the git summary task failed: {error}"))?;

    Ok(Some(GitSummary {
        files: files + untracked_files,
        additions: additions + untracked_lines,
        deletions,
    }))
}

/// The directory a session's cwd names, if it is one this host may ask Git
/// about.
///
/// An empty or relative value must never become the process's own working
/// directory: `git` would then answer for whatever directory the host happened
/// to be started in, which is not the session's workspace.
async fn trusted_dir(cwd: &str) -> Option<PathBuf> {
    let path = Path::new(cwd);
    if cwd.is_empty() || !path.is_absolute() {
        return None;
    }
    tokio::fs::metadata(path)
        .await
        .ok()?
        .is_dir()
        .then(|| path.to_path_buf())
}

/// The work-tree root Git finds from `dir`, or `None` when there is none.
///
/// Git answers "not a git repository" both for a plain directory and for a
/// repository whose own metadata is unreadable. The walk only supplies that
/// distinction — the root itself still comes from Git — so a broken repository
/// is reported as a failed read instead of as "no repository here".
async fn discover(dir: &Path) -> Result<Option<PathBuf>, String> {
    let ran = run(dir, &["rev-parse", "--show-toplevel"]).await?;
    if ran.success {
        let text = String::from_utf8_lossy(&ran.stdout);
        let root = text.trim_end_matches(&['\r', '\n'][..]);
        if root.is_empty() {
            return Ok(None);
        }
        let root = PathBuf::from(root);
        // A relative answer would make every later command depend on this
        // process's directory; there is nothing safe to do with it.
        return Ok(root.is_absolute().then_some(root));
    }
    if ran.stderr.contains("must be run in a work tree") {
        // A bare repository: there is no working tree to count.
        return Ok(None);
    }
    if ran.stderr.contains("not a git repository") && !holds_a_repository(dir).await {
        return Ok(None);
    }
    Err(failed("rev-parse --show-toplevel", dir, &ran))
}

/// Whether any directory from `dir` up holds a `.git` entry.
///
/// Discovery itself is Git's; this is only what tells a plain directory apart
/// from a repository Git refused to read.
async fn holds_a_repository(dir: &Path) -> bool {
    let mut current = Some(dir);
    while let Some(path) = current {
        if tokio::fs::symlink_metadata(path.join(".git")).await.is_ok() {
            return true;
        }
        current = path.parent();
    }
    false
}

/// One read-only Git invocation.
struct Ran {
    success: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

async fn run(dir: &Path, args: &[&str]) -> Result<Ran, String> {
    run_with(dir, args, &[]).await
}

/// One read-only Git invocation with extra command-scope configuration.
///
/// The configuration is passed as `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/
/// `GIT_CONFIG_VALUE_n` rather than `-c`: a filter name is a configuration
/// subsection and may itself contain `=`, which `-c name=value` cannot express
/// without the value boundary moving.
async fn run_with(dir: &Path, args: &[&str], config: &[(String, String)]) -> Result<Ran, String> {
    // `-c` is understood by every Git; the environment form is used only for a
    // key that `-c` cannot spell, which needs Git 2.31 or newer.
    let (plain, encoded): (Vec<_>, Vec<_>) = config.iter().partition(|(key, _)| !key.contains('='));
    let mut command = agi_platform::command(Path::new("git"));
    command.arg("-C").arg(dir);
    for (key, value) in plain {
        command.arg("-c").arg(format!("{key}={value}"));
    }
    command
        // A read must not become an execution: `diff.external` and a fsmonitor
        // hook are programs a repository may name, and neither may run because
        // a phone refreshed a pill.
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .arg("--no-pager")
        .args(args)
        // No optional lock: this must never write the index or hold
        // `index.lock` while the operator's own Git is running.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        // A partial clone must answer from what it has: a refresh may not go to
        // the network for a missing object.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        // The host's environment may name a different repository; discovery
        // must start at the session's directory and nowhere else.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_EXTERNAL_DIFF")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timeout that leaves Git running would let the next refresh race it.
        .kill_on_drop(true);
    // Configuration inherited from the environment is neither the repository's
    // nor this module's: drop it so the keys read below are the same ones the
    // diff will see. Our own overrides are added afterwards.
    strip_config_environment(&mut command);
    if !encoded.is_empty() {
        command.env("GIT_CONFIG_COUNT", encoded.len().to_string());
        for (index, (key, value)) in encoded.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
    }
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "git {} did not answer within {} seconds",
                args.join(" "),
                GIT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| format!("git could not be run: {error}"))?;
    Ok(Ran {
        success: output.status.success(),
        code: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Remove environment-borne configuration (`git -c` and `GIT_CONFIG_*`) from a
/// command that inherits this process's environment.
fn strip_config_environment(command: &mut tokio::process::Command) {
    for name in [
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let numbered = name.to_str().is_some_and(|name| {
            name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_")
        });
        if numbered {
            command.env_remove(name);
        }
    }
}

/// Disable every clean filter the effective configuration defines.
///
/// The keys are read from the configuration Git will use for the diff (the
/// query itself is read-only), and an unset `filter.<driver>.required` is
/// forced to `false` so an empty command is a pass-through rather than an
/// error. Exit code 1 means "no such keys"; any other failure is reported
/// instead of diffing with a filter this side could not see.
async fn filter_overrides(root: &Path) -> Result<Vec<(String, String)>, String> {
    let listed = run(
        root,
        &[
            "config",
            "--null",
            "--name-only",
            "--includes",
            "--get-regexp",
            r"^filter\..*\.(clean|process|required)$",
        ],
    )
    .await?;
    if !listed.success {
        // Exit 1 with no output is Git's "no key matched"; anything else is a
        // configuration this side could not read, so the diff must not run.
        if listed.code == Some(1) && listed.stdout.is_empty() {
            return Ok(Vec::new());
        }
        return Err(failed("config", root, &listed));
    }
    let mut drivers = BTreeSet::new();
    for key in split_nul(&listed.stdout) {
        // A key this side cannot name is a key it cannot override.
        let key = std::str::from_utf8(key)
            .map_err(|_| "git config named a filter this host cannot read".to_string())?;
        let Some(rest) = key.strip_prefix("filter.") else {
            continue;
        };
        let name = [".clean", ".process", ".required"]
            .iter()
            .find_map(|suffix| rest.strip_suffix(suffix));
        if let Some(name) = name {
            if !name.is_empty() {
                drivers.insert(name.to_string());
            }
        }
    }
    let mut config = Vec::with_capacity(drivers.len() * 3);
    for name in drivers {
        for (suffix, value) in [("clean", ""), ("process", ""), ("required", "false")] {
            config.push((format!("filter.{name}.{suffix}"), value.to_string()));
        }
    }
    Ok(config)
}

/// The message a failed invocation is reported with.
///
/// Git's own stderr may name the repository's path, and the phone is never sent
/// paths: the detail is logged on the host, the answer carries only the failure.
fn failed(what: &str, dir: &Path, ran: &Ran) -> String {
    tracing::warn!(
        "mizar: git {what} in {} failed ({:?}): {}",
        dir.display(),
        ran.code,
        ran.stderr
    );
    format!("git {what} failed")
}

/// Fields of a `-z` listing, without the trailing empty one.
fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
}

/// One path from a `-z` listing.
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// On Windows Git writes UTF-8 names; an undecodable one still counts as a file
/// (the numbers come from Git) but its lines are not read.
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

/// Count `git diff --numstat -z` entries and their lines.
///
/// One entry is `adds TAB dels TAB path NUL`; a rename writes an empty path and
/// then the old and new names as two more fields, so it counts as one file.
fn parse_numstat(bytes: &[u8]) -> (u64, u64, u64) {
    let mut files = 0u64;
    let mut additions = 0u64;
    let mut deletions = 0u64;
    let mut fields = bytes.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let mut parts = field.splitn(3, |byte| *byte == b'\t');
        let (Some(adds), Some(dels), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        files += 1;
        additions += number(adds);
        deletions += number(dels);
        if path.is_empty() {
            // Rename: the old and the new name follow as their own fields.
            fields.next();
            fields.next();
        }
    }
    (files, additions, deletions)
}

/// A count field; binary files are written as `-` and count only as a file.
fn number(field: &[u8]) -> u64 {
    std::str::from_utf8(field)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(0)
}

/// Count untracked files and the text lines they would add.
///
/// Every listed entry is a file for the count. A regular file's lines are read
/// only while they fit the budget; symlinks are counted and never followed,
/// because reading through one would turn a refresh into a read of a file
/// outside the repository — the same thing the file fence refuses.
fn count_untracked(root: &Path, paths: &[PathBuf]) -> (u64, u64) {
    let mut budget = UNTRACKED_TOTAL_MAX;
    let mut additions = 0u64;
    for relative in paths {
        let full = root.join(relative);
        // `ls-files --others` also names a directory that is itself an untracked
        // repository; it is one entry, not a tree to walk.
        let Ok(metadata) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let length = metadata.len();
        if length > UNTRACKED_FILE_MAX || length > budget {
            continue;
        }
        // The budget bounds work, not just text: a file that turns out binary
        // has still been read to its NUL, so it is charged before the read.
        budget -= length;
        if let Some(lines) = text_lines(&full, length) {
            additions += lines;
        }
    }
    (paths.len() as u64, additions)
}

/// Lines a text file holds, counted the way Git counts them: a last line
/// without a newline still counts. `None` when the bytes are binary (contain a
/// NUL) or unreadable.
fn text_lines(path: &Path, length: u64) -> Option<u64> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut reader = file.take(length);
    let mut buffer = [0u8; 64 * 1024];
    let mut lines = 0u64;
    let mut last = 0u8;
    let mut seen = false;
    loop {
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        if chunk.contains(&0) {
            return None;
        }
        seen = true;
        last = chunk[read - 1];
        lines += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
    }
    if seen && last != b'\n' {
        lines += 1;
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run Git the way a test needs it: with a repository-local identity and
    /// without the optional locks this module also refuses.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .expect("git is installed");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn init(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path());
        dir
    }

    fn write(dir: &Path, name: &str, contents: &[u8]) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn commit(dir: &Path, message: &str) {
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", message]);
    }

    async fn summary(dir: &Path) -> Option<GitSummary> {
        summarize(dir.to_str().expect("a temporary path is UTF-8"))
            .await
            .expect("git answers")
    }

    fn counts(files: u64, additions: u64, deletions: u64) -> Option<GitSummary> {
        Some(GitSummary {
            files,
            additions,
            deletions,
        })
    }

    #[tokio::test]
    async fn a_plain_directory_is_no_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(summary(dir.path()).await, None);
    }

    #[tokio::test]
    async fn empty_relative_and_file_cwds_are_never_asked_about() {
        // Each of these could otherwise become the process's own directory.
        assert_eq!(summarize("").await.unwrap(), None);
        assert_eq!(summarize("crates").await.unwrap(), None);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x\n").unwrap();
        assert_eq!(summarize(file.to_str().unwrap()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_clean_repository_is_three_zeros() {
        let dir = repo();
        write(dir.path(), "a.txt", b"one\ntwo\n");
        commit(dir.path(), "base");
        assert_eq!(summary(dir.path()).await, counts(0, 0, 0));
    }

    #[tokio::test]
    async fn staged_and_unstaged_edits_count_once() {
        let dir = repo();
        write(dir.path(), "a.txt", b"a\nb\nc\n");
        commit(dir.path(), "base");
        // One staged line, then a second edit on top of it: the file appears
        // once, with the net change against HEAD.
        write(dir.path(), "a.txt", b"a\nb\nc\nd\n");
        git(dir.path(), &["add", "a.txt"]);
        write(dir.path(), "a.txt", b"a\nb\nc\nd\ne\n");
        assert_eq!(summary(dir.path()).await, counts(1, 2, 0));
    }

    #[tokio::test]
    async fn a_deleted_file_counts_its_lines() {
        let dir = repo();
        write(dir.path(), "gone.txt", b"one\ntwo\nthree\n");
        commit(dir.path(), "base");
        std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
        assert_eq!(summary(dir.path()).await, counts(1, 0, 3));
    }

    #[tokio::test]
    async fn untracked_text_counts_even_without_a_final_newline() {
        let dir = repo();
        write(dir.path(), "kept.txt", b"kept\n");
        commit(dir.path(), "base");
        write(dir.path(), "u.txt", b"x\ny");
        write(dir.path(), "empty.txt", b"");
        // The empty file is still one file; `x\ny` is two lines without a
        // trailing newline.
        assert_eq!(summary(dir.path()).await, counts(2, 2, 0));
    }

    #[tokio::test]
    async fn ignored_files_are_not_untracked_files() {
        let dir = repo();
        write(dir.path(), ".gitignore", b"*.log\n");
        commit(dir.path(), "ignore logs");
        write(dir.path(), "debug.log", b"a\nb\n");
        write(dir.path(), "u.txt", b"z\n");
        assert_eq!(summary(dir.path()).await, counts(1, 1, 0));
    }

    #[tokio::test]
    async fn binary_changes_count_as_a_file_only() {
        let dir = repo();
        write(dir.path(), "b.bin", b"\x00\x01\x02");
        commit(dir.path(), "base");
        write(dir.path(), "b.bin", b"\x00\x03");
        write(dir.path(), "u.bin", b"\x00\x01");
        assert_eq!(summary(dir.path()).await, counts(2, 0, 0));
    }

    #[tokio::test]
    async fn an_empty_repository_diffs_against_the_empty_tree() {
        let dir = repo();
        // No commit: the staged file is an addition against the empty tree, and
        // the untracked one is an addition of its own.
        write(dir.path(), "a.txt", b"one\ntwo\n");
        git(dir.path(), &["add", "a.txt"]);
        write(dir.path(), "u.txt", b"three\n");
        assert_eq!(summary(dir.path()).await, counts(2, 3, 0));
    }

    #[tokio::test]
    async fn a_subdirectory_session_sees_the_whole_repository() {
        let dir = repo();
        write(dir.path(), "a.txt", b"one\n");
        write(dir.path(), "sub/s.txt", b"s\n");
        commit(dir.path(), "base");
        write(dir.path(), "a.txt", b"one\ntwo\n");
        write(dir.path(), "sub/u.txt", b"u\n");
        assert_eq!(summary(&dir.path().join("sub")).await, counts(2, 2, 0));
    }

    #[tokio::test]
    async fn a_linked_worktree_reports_its_own_changes() {
        let holder = tempfile::tempdir().unwrap();
        let main = holder.path().join("main");
        std::fs::create_dir(&main).unwrap();
        init(&main);
        write(&main, "a.txt", b"one\n");
        commit(&main, "base");
        let tree = holder.path().join("tree");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                tree.to_str().unwrap(),
                "-b",
                "other",
            ],
        );
        assert_eq!(summary(&main).await, counts(0, 0, 0));
        write(&tree, "a.txt", b"one\ntwo\n");
        assert_eq!(summary(&tree).await, counts(1, 1, 0));
        // The main checkout is still clean: the worktree is its own working
        // tree, not a second view of the same files.
        assert_eq!(summary(&main).await, counts(0, 0, 0));
    }

    #[tokio::test]
    async fn the_nearest_repository_wins() {
        let dir = repo();
        write(dir.path(), "a.txt", b"one\n");
        commit(dir.path(), "outer");
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        init(&inner);
        write(&inner, "i.txt", b"i\n");
        commit(&inner, "inner");
        write(dir.path(), "a.txt", b"one\ntwo\n");
        write(&inner, "i.txt", b"i\nj\n");
        // The session sits in the nested repository, so the outer edit is not
        // part of its answer.
        assert_eq!(summary(&inner).await, counts(1, 1, 0));
    }

    #[tokio::test]
    async fn a_rename_is_one_file() {
        let dir = repo();
        write(dir.path(), "a.txt", b"a\nb\n");
        commit(dir.path(), "base");
        git(dir.path(), &["mv", "a.txt", "b.txt"]);
        assert_eq!(summary(dir.path()).await, counts(1, 0, 0));
        write(dir.path(), "b.txt", b"a\nb\nc\n");
        assert_eq!(summary(dir.path()).await, counts(1, 1, 0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_untracked_symlink_is_counted_but_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, b"s1\ns2\ns3\n").unwrap();
        let dir = repo();
        write(dir.path(), "kept.txt", b"kept\n");
        commit(dir.path(), "base");
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        // The symlink is one untracked file; its target's three lines belong to
        // a file outside the repository and are not counted.
        assert_eq!(summary(dir.path()).await, counts(1, 0, 0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_repositorys_own_hooks_diff_driver_and_clean_filters_never_run() {
        let dir = repo();
        write(dir.path(), "a.txt", b"a\n");
        write(dir.path(), "b.txt", b"b\n");
        write(dir.path(), "c.txt", b"c\n");
        commit(dir.path(), "base");
        // The three filters are named by attributes committed separately, so
        // the setup's own `git add` never invokes them.
        write(
            dir.path(),
            ".gitattributes",
            b"a.txt filter=cleaned\nb.txt filter=processed\nc.txt filter=a=b\n",
        );
        git(dir.path(), &["add", ".gitattributes"]);
        git(dir.path(), &["commit", "-qm", "attributes"]);
        // The script and its marker live outside the repository, so they cannot
        // show up in the answer as untracked files.
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("MARKER");
        let script = outside.path().join("hook.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\nprintf ''\n",
                marker.to_str().unwrap()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            dir.path(),
            &["config", "core.fsmonitor", script.to_str().unwrap()],
        );
        git(
            dir.path(),
            &["config", "diff.external", script.to_str().unwrap()],
        );
        // A clean command, a long-lived process filter, and a driver whose name
        // contains `=` — the last one is why overrides travel through
        // `GIT_CONFIG_COUNT` instead of `-c`.
        git(
            dir.path(),
            &["config", "filter.cleaned.clean", script.to_str().unwrap()],
        );
        git(
            dir.path(),
            &[
                "config",
                "filter.processed.process",
                script.to_str().unwrap(),
            ],
        );
        git(
            dir.path(),
            &["config", "filter.a=b.clean", script.to_str().unwrap()],
        );
        write(dir.path(), "a.txt", b"a\nb\n");
        write(dir.path(), "b.txt", b"b\nb\n");
        write(dir.path(), "c.txt", b"c\nc\n");
        assert_eq!(summary(dir.path()).await, counts(3, 3, 0));
        assert!(
            !marker.exists(),
            "a repository hook or filter ran while answering a read"
        );
        assert!(!dir.path().join(".git/index.lock").exists());
    }

    #[tokio::test]
    async fn a_broken_repository_is_an_error_not_an_empty_answer() {
        let dir = repo();
        write(dir.path(), "a.txt", b"a\n");
        commit(dir.path(), "base");
        // Git cannot tell this apart from "not a repository" by exit code alone;
        // the answer must still be a failure, not "there is nothing here".
        std::fs::write(dir.path().join(".git/HEAD"), b"garbage\n").unwrap();
        assert!(summarize(dir.path().to_str().unwrap()).await.is_err());
    }

    #[test]
    fn a_rename_entry_parses_as_one_file() {
        // `1\t0\t\0old\0new\0`: the empty path field marks a rename.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"1\t0\t\0a.txt\0b.txt\0");
        bytes.extend_from_slice(b"2\t1\tnew.txt\0");
        assert_eq!(parse_numstat(&bytes), (2, 3, 1));
    }
}
