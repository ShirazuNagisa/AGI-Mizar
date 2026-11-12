//! Live check against the real model sources. Not part of the suite.
//!
//! It needs the network, a published mirror and a gigabyte of disk, so it is
//! `#[ignore]`d like the sidecar test. Run it when the source list or the
//! request headers change, because the interesting failures here are invisible
//! to a mock server:
//!
//! ```sh
//! cargo test -p agi-core --test live_modelscope -- --ignored --nocapture
//! ```
//!
//! The last time it was run (2026-11-12) the mirror served 1056 MiB in 106 s and
//! the first version of this test caught a 403: ModelScope's edge rejects
//! requests with no `User-Agent`, which a mock never does.

use std::time::{Duration, Instant};

use agi_core::models::ModelService;

#[tokio::test]
#[ignore = "downloads the published model (about 1 GB)"]
async fn the_mirror_serves_the_release_the_client_expects() {
    let dir = tempfile::tempdir().unwrap();
    let service = ModelService::open(dir.path().to_path_buf());

    let check = service.check().await;
    let latest = check.latest.expect("a release should be offered");
    println!("check: {} -> {}", latest.version, latest.url);
    assert!(
        latest.url.contains("modelscope.cn"),
        "the first source must win, got {}",
        latest.url
    );
    let expected = latest.size_bytes.expect("the manifest states a size");
    assert!(expected > 1_000_000_000, "the real weights are ~1 GB");

    service.start_download(None).expect("accepted");
    let started = Instant::now();
    let mut last = 0;
    loop {
        let state = service.state();
        match state.download.as_ref() {
            Some(download) if download.state == "failed" => {
                panic!("download failed: {:?}", download.error)
            }
            Some(download) => {
                if let Some(source) = download.source.as_deref() {
                    assert_eq!(source, "ModelScope", "the first source must win");
                }
                if download.done / (128 << 20) != last / (128 << 20) {
                    println!(
                        "{:?} {:.0} / {:.0} MiB",
                        download.source,
                        download.done as f64 / 1048576.0,
                        download.total as f64 / 1048576.0
                    );
                }
                last = download.done;
            }
            None => {
                let active = state.active.expect("a model is installed");
                println!(
                    "installed {} ({} B) in {:.1}s",
                    active.version,
                    active.size_bytes,
                    started.elapsed().as_secs_f64()
                );
                assert_eq!(active.version, latest.version);
                assert_eq!(active.size_bytes, expected);
                assert!(dir.path().join(format!("{}.json", active.id)).exists());
                let partials = std::fs::read_dir(dir.path().join(".download"))
                    .map(|entries| entries.filter_map(|entry| entry.ok()).count())
                    .unwrap_or(0);
                assert_eq!(partials, 0, "a finished download leaves no partial file");
                return;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(900),
            "timed out after {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
