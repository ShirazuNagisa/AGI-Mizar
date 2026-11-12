//! End-to-end tests for the model downloader, against a local server.
//!
//! The interesting failures are not "does it download": they are "does it resume",
//! "does it refuse a file whose bytes are not the ones the manifest promised",
//! and "does a second download start while one is running". Each of those needs a
//! real HTTP response with real headers, so this spins up the kernel's own axum
//! and serves a few kilobytes that pretend to be a 1 GB model.

use std::sync::Arc;

use agi_core::models::{ModelService, Source};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};

#[derive(Clone)]
struct Fixture {
    bytes: Arc<Vec<u8>>,
    /// What the manifest claims the checksum is.
    sha256: String,
    /// Whether to honour `Range`.
    range: bool,
    /// Delay before the first byte, so "while one is running" is testable.
    delay_ms: u64,
    /// The release this server pretends to be.
    version: &'static str,
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

async fn manifest(State(fixture): State<Fixture>) -> Response {
    let body = serde_json::json!({
        "id": "mizarllm-v1",
        "version": fixture.version,
        "backend": { "kind": "http", "endpoint": "http://127.0.0.1:0", "timeout_ms": 1500 },
        "labels": { "done": 11, "needs_user": 4, "stalled": 9, "waiting": 8 },
        "note": "test",
        "file": "mizarllm-1.0.0-q4km.gguf",
        "sha256": fixture.sha256,
        "size_bytes": fixture.bytes.len(),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn asset(State(fixture): State<Fixture>, headers: HeaderMap) -> Response {
    if fixture.delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(fixture.delay_ms)).await;
    }
    let total = fixture.bytes.len();
    let start = if fixture.range {
        headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split('-').next())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let body = fixture.bytes[start.min(total)..].to_vec();
    let status = if fixture.range && start > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/octet-stream")
        .header("content-length", body.len())
        .header("accept-ranges", "bytes")
        .header("etag", "\"fixture\"")
        .body(Body::from(body))
        .unwrap()
}

/// Serve the fixture and return the base URL the service should use.
async fn serve(fixture: Fixture) -> String {
    let asset_path = format!(
        "/releases/download/{}/mizarllm-1.0.0-q4km.gguf",
        fixture.version
    );
    let app = Router::new()
        .route("/releases/latest/download/manifest.json", get(manifest))
        .route(&asset_path, get(asset))
        .with_state(fixture);
    listen(app).await
}

/// A repository that has the manifest and no files: the mirror that was half
/// uploaded, or the one whose storage went away.
async fn serve_manifest_only(fixture: Fixture) -> String {
    let app = Router::new()
        .route("/releases/latest/download/manifest.json", get(manifest))
        .with_state(fixture);
    listen(app).await
}

/// Serve one router on a port the kernel picks, and return its base URL.
async fn listen(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Wait for the download to finish, one way or another.
///
/// The task is spawned, so the first look may still be before it registered
/// itself; "no download in flight" only means finished once one was seen.
async fn settle(service: &Arc<ModelService>) -> String {
    for _ in 0..400 {
        let state = service.state();
        match state.download {
            Some(download) if download.state == "failed" => {
                return download.error.unwrap_or_default()
            }
            // No frame and a model on disk: the task finished between polls.
            None if state.active.is_some() => return String::new(),
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    "timed out".to_string()
}

#[tokio::test]
async fn a_download_lands_whole_and_becomes_the_active_model() {
    let bytes = payload(64 * 1024);
    let base = serve(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes.clone()),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    // Each test has its own fixture and its own base: a process-wide variable
    // made four tests overwrite each other's answer under CI's parallelism.
    service.point_at(base.as_str());
    let check = service.check().await;
    assert!(
        check.latest.is_some(),
        "the repository could not be read: {:?} (base {base})",
        check.error
    );
    service.start_download(None).expect("accepted");
    assert_eq!(settle(&service).await, "", "the download should succeed");

    let state = service.state();
    let active = state.active.expect("a model is active");
    assert_eq!(active.version, "v1");
    assert_eq!(state.runtime.state, "stopped");

    let installed = std::fs::read(dir.path().join("mizarllm-v1.gguf")).unwrap();
    assert_eq!(installed, bytes, "the file on disk is the one served");
    assert!(dir.path().join("mizarllm-v1.json").exists());
    // The temporary file is gone: a half-written download must never look whole.
    assert!(!dir
        .path()
        .join(".download/mizarllm-1.0.0-q4km.gguf.part")
        .exists());
}

#[tokio::test]
async fn a_wrong_checksum_leaves_nothing_behind() {
    let bytes = payload(8 * 1024);
    let base = serve(Fixture {
        sha256: digest(b"not the same bytes"),
        bytes: Arc::new(bytes),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    // Each test has its own fixture and its own base: a process-wide variable
    // made four tests overwrite each other's answer under CI's parallelism.
    service.point_at(base.as_str());
    service.start_download(None).expect("accepted");
    let error = settle(&service).await;
    assert!(
        error.contains("checksum"),
        "expected a checksum error, got {error}"
    );

    assert!(service.state().active.is_none());
    assert!(!dir.path().join("mizarllm-v1.gguf").exists());
}

#[tokio::test]
async fn a_partial_file_resumes_instead_of_starting_over() {
    let bytes = payload(32 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let partial = bytes[..16 * 1024].to_vec();
    let part_dir = dir.path().join(".download");
    std::fs::create_dir_all(&part_dir).unwrap();
    std::fs::write(part_dir.join("mizarllm-1.0.0-q4km.gguf.part"), &partial).unwrap();

    let base = serve(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes.clone()),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let service = ModelService::open(dir.path().to_path_buf());
    // Each test has its own fixture and its own base: a process-wide variable
    // made four tests overwrite each other's answer under CI's parallelism.
    service.point_at(base.as_str());
    service.start_download(None).expect("accepted");
    assert_eq!(settle(&service).await, "");

    let installed = std::fs::read(dir.path().join("mizarllm-v1.gguf")).unwrap();
    assert_eq!(
        installed, bytes,
        "the resumed file must not repeat the first half"
    );
}

#[tokio::test]
async fn a_second_download_is_refused_while_one_runs() {
    let bytes = payload(4 * 1024);
    let base = serve(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes),
        range: true,
        delay_ms: 400,
        version: "v1",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    // Each test has its own fixture and its own base: a process-wide variable
    // made four tests overwrite each other's answer under CI's parallelism.
    service.point_at(base.as_str());
    service.start_download(None).expect("first accepted");
    // Give the task a moment to claim the slot.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let second = service.start_download(None);
    assert!(
        second.is_err(),
        "two writers to one file is how a download gets corrupted"
    );
    let _ = settle(&service).await;
}

/// Rolling back must put the old bytes back, not the new ones under an old name.
///
/// The first version of this code renamed `<id>.gguf` over `<id>.gguf` and then
/// renamed it back: the "previous" weights were the current ones, so a rollback
/// looked right in the manifest and was wrong on disk.
#[tokio::test]
async fn a_rollback_restores_the_previous_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".previous")).unwrap();

    let v1 = payload(2048);
    let v2: Vec<u8> = v1.iter().map(|byte| byte.wrapping_add(9)).collect();

    std::fs::write(root.join("mizarllm-v1.gguf"), &v2).unwrap();
    std::fs::write(
        root.join("mizarllm-v1.json"),
        serde_json::json!({
            "id": "mizarllm-v1", "version": "2.0.0",
            "backend": { "kind": "http", "endpoint": "http://127.0.0.1:0", "timeout_ms": 1500 },
            "labels": { "done": 11, "needs_user": 4, "stalled": 9, "waiting": 8 }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join(".previous/mizarllm-v1-1.0.0.gguf"), &v1).unwrap();
    std::fs::write(
        root.join(".previous/manifest.json"),
        serde_json::json!({
            "id": "mizarllm-v1", "version": "1.0.0",
            "backend": { "kind": "http", "endpoint": "http://127.0.0.1:0", "timeout_ms": 1500 },
            "labels": { "done": 11, "needs_user": 4, "stalled": 9, "waiting": 8 }
        })
        .to_string(),
    )
    .unwrap();

    let service = ModelService::open(root.clone());
    assert_eq!(service.manifest().unwrap().version, "2.0.0");

    let restored = service.rollback().expect("a rollback");
    assert_eq!(restored.version, "1.0.0");
    assert_eq!(
        std::fs::read(root.join("mizarllm-v1.gguf")).unwrap(),
        v1,
        "the bytes on disk must be the previous model's"
    );
    assert_eq!(
        std::fs::read(root.join(".previous/mizarllm-v1-2.0.0.gguf")).unwrap(),
        v2,
        "and the model just replaced must be the one kept aside"
    );
    assert_eq!(service.state().active.unwrap().version, "1.0.0");
}

/// A source that cannot be reached is a fallback, not a failure.
///
/// The reason this exists: the release CDN answers in tens of kilobytes per
/// second from mainland China while a mirror answers in megabytes, so the
/// client tries the mirror first and must survive it being down.
#[tokio::test]
async fn a_dead_source_falls_back_to_the_next() {
    let bytes = payload(16 * 1024);
    let live = serve(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes.clone()),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    service.route_sources(vec![
        // Nothing listens on port 1: connection refused, immediately.
        Source::release("dead", "http://127.0.0.1:1"),
        Source::release("live", &live),
    ]);

    let check = service.check().await;
    let latest = check.latest.expect("the live source should answer");
    assert_eq!(latest.version, "v1");
    assert!(
        latest.url.contains(&live),
        "the URL must come from the live source"
    );

    service.start_download(None).expect("accepted");
    assert_eq!(settle(&service).await, "");
    assert_eq!(
        std::fs::read(dir.path().join("mizarllm-v1.gguf")).unwrap(),
        bytes
    );
}

/// A mirror with the manifest but not the file falls through to the next source.
#[tokio::test]
async fn a_source_without_the_file_falls_back_to_the_next() {
    let bytes = payload(16 * 1024);
    let empty = serve_manifest_only(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes.clone()),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let full = serve(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes.clone()),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    service.route_sources(vec![
        Source::release("empty", &empty),
        Source::release("full", &full),
    ]);

    service.start_download(None).expect("accepted");
    assert_eq!(settle(&service).await, "");
    assert_eq!(
        std::fs::read(dir.path().join("mizarllm-v1.gguf")).unwrap(),
        bytes
    );
}

/// Bytes that fail the manifest's checksum are the wrong model, not a head
/// start: the partial is dropped and the next source starts from zero.
#[tokio::test]
async fn a_source_with_the_wrong_bytes_falls_through() {
    let bytes = payload(16 * 1024);
    let wrong: Vec<u8> = bytes.iter().map(|byte| byte.wrapping_add(7)).collect();
    let bad = serve(Fixture {
        // The manifest promises the right bytes; the server sends others.
        sha256: digest(&bytes),
        bytes: Arc::new(wrong),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let good = serve(Fixture {
        sha256: digest(&bytes),
        bytes: Arc::new(bytes.clone()),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    service.route_sources(vec![
        Source::release("bad", &bad),
        Source::release("good", &good),
    ]);

    service.start_download(None).expect("accepted");
    assert_eq!(settle(&service).await, "");
    assert_eq!(
        std::fs::read(dir.path().join("mizarllm-v1.gguf")).unwrap(),
        bytes,
        "the good source must be fetched from scratch, not resumed onto bad bytes"
    );
}

/// The newest manifest wins, wherever it came from: a mirror left behind on the
/// previous version must not pin the client to it.
#[tokio::test]
async fn the_newest_manifest_wins_across_sources() {
    let old = payload(8 * 1024);
    let new: Vec<u8> = payload(12 * 1024)
        .iter()
        .map(|byte| byte.wrapping_add(3))
        .collect();
    let mirror = serve(Fixture {
        sha256: digest(&old),
        bytes: Arc::new(old),
        range: true,
        delay_ms: 0,
        version: "v1",
    })
    .await;
    let release = serve(Fixture {
        sha256: digest(&new),
        bytes: Arc::new(new.clone()),
        range: true,
        delay_ms: 0,
        version: "v2",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());
    service.route_sources(vec![
        Source::release("mirror", &mirror),
        Source::release("release", &release),
    ]);

    let check = service.check().await;
    let latest = check.latest.expect("one of the two should answer");
    assert_eq!(latest.version, "v2", "the newer manifest must win");
    assert!(latest.newer);

    service.start_download(None).expect("accepted");
    assert_eq!(settle(&service).await, "");
    let state = service.state();
    assert_eq!(state.active.expect("a model is active").version, "v2");
    assert_eq!(
        std::fs::read(dir.path().join("mizarllm-v1.gguf")).unwrap(),
        new
    );
}

/// The real thing: start `llama-server` with the real weights and ask it.
///
/// Ignored by default: it needs the 1 GB model and the bundled runner, neither
/// of which belongs in a unit test. Run it with
/// `cargo test -p agi-core --test models -- --ignored` on a machine that has
/// both.
#[tokio::test]
#[ignore = "needs the real weights and llama-server"]
async fn the_sidecar_starts_and_stops() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../models/mizarllm-v1-q4km.gguf");
    if !workspace.exists() {
        eprintln!("no weights at {}, skipping", workspace.display());
        return;
    }
    // The runner ships as a plain executable on macOS and as `.exe` on Windows;
    // the packaged layout puts it under the app's resources/bin in both cases.
    let runner_name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agi-app/resources/bin")
        .join(runner_name);
    assert!(
        runner.exists(),
        "run scripts/fetch-llama.sh first (looked for {})",
        runner.display()
    );
    std::env::set_var("AGI_LLAMA_BIN", &runner);

    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&workspace, dir.path().join("mizarllm-v1.gguf")).unwrap();
    #[cfg(windows)]
    std::fs::copy(&workspace, dir.path().join("mizarllm-v1.gguf")).unwrap();
    std::fs::write(
        dir.path().join("mizarllm-v1.json"),
        serde_json::json!({
            "id": "mizarllm-v1",
            "version": "1.0.0",
            "backend": { "kind": "http", "endpoint": "http://127.0.0.1:0", "timeout_ms": 1500 },
            "labels": { "done": 11, "needs_user": 4, "stalled": 9, "waiting": 8 }
        })
        .to_string(),
    )
    .unwrap();

    let service = ModelService::open(dir.path().to_path_buf());
    assert!(
        service.manifest().is_some(),
        "the manifest should be adopted"
    );

    let endpoint = service.ensure_endpoint().await.expect("the sidecar starts");
    let health = reqwest::get(format!("{endpoint}/health")).await.unwrap();
    assert!(health.status().is_success());
    assert_eq!(service.state().runtime.state, "ready");

    service.shutdown().await;
    assert_eq!(service.state().runtime.state, "stopped");
}
