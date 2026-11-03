//! The Rust prompt builder must stay byte-identical to the training pipeline.
//!
//! `fixture_facts.json`, `fixture_prompt.txt` and `fixture_system.txt` were
//! captured from `tools/replay/teacher/` (`prompt.md` / `format.mjs`) using a
//! real training row. If a test here ever fails, the kernel is feeding the model
//! text it was never trained on — retrain or fix the renderer, never "adjust the
//! test".

use agi_layout::{render_prompt, teacher_system_prompt, Facts};

#[test]
fn rendered_prompt_matches_the_training_pipeline() {
    let facts: Facts =
        serde_json::from_str(include_str!("fixture_facts.json")).expect("fixture facts parse");
    let expected = include_str!("fixture_prompt.txt");

    let got = render_prompt(&facts);
    assert_eq!(
        got.trim_end(),
        expected.trim_end(),
        "\n--- rust ---\n{got}\n--- training ---\n{expected}\n"
    );
}

/// The `system` message `dataset.mjs` trains with is `prompt.md.trim()`; the
/// judge must send that exact string, not a paraphrase.
#[test]
fn the_system_prompt_matches_the_training_pipeline() {
    let expected = include_str!("fixture_system.txt");

    let got = teacher_system_prompt();
    assert_eq!(
        got,
        expected.trim(),
        "\n--- rust ---\n{got:?}\n--- training ---\n{:?}\n",
        expected.trim()
    );
}
