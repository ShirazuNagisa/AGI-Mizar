//! The Rust prompt builder must stay byte-identical to the training pipeline.
//!
//! `fixture_facts.json` and `fixture_prompt.txt` were produced by
//! `tools/teacher/format.mjs` from a real training row. If this test ever fails,
//! the kernel is feeding the model text it was never trained on — retrain or fix
//! the renderer, never "adjust the test".

use agi_layout::{render_prompt, Facts};

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
