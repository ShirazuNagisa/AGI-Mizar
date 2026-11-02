//! The golden frames in `protocol/fixtures/` are the contract the phones test
//! against, so the host's answer has to be that shape — not merely a shape the
//! current clients happen to tolerate.
//!
//! This file pins the one that drifted: `session.messages` answers with
//! `records[]`, and every element is an **event envelope**
//! `{"type":"event","event":{type,seq,time,data}}`, not a bare event. The phones
//! decode both (`payload = event ?? raw`), which is exactly why the host could
//! be wrong for a while without anybody noticing.

use agi_mizar::frames::{record_to_envelope, record_to_event, ServerFrame};
use agi_protocol::domain::{Record, RecordKind};
use serde_json::{json, Value};

/// The golden `session.messages` answer the Android and iOS protocol tests use.
const SESSION_MESSAGES: &str = include_str!("../../../protocol/fixtures/session-messages.json");

fn golden() -> Value {
    serde_json::from_str(SESSION_MESSAGES).expect("the golden fixture is valid JSON")
}

/// One record of the kinds the conversation summary carries.
fn record(seq: i64, time: i64, turn: u32, kind: RecordKind, text: Option<&str>) -> Record {
    Record {
        seq,
        time,
        turn: Some(turn),
        step: None,
        kind,
        text: text.map(str::to_string),
        tool: None,
    }
}

#[test]
fn the_golden_records_are_event_envelopes() {
    let fixture = golden();
    assert_eq!(fixture["v"], 1);
    assert_eq!(fixture["type"], "ok");
    let records = fixture["result"]["records"]
        .as_array()
        .expect("result.records[]");
    assert!(!records.is_empty(), "the fixture has to show the shape");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["type"], "event", "records[{index}] is an envelope");
        let event = record.get("event").expect("an envelope carries an event");
        assert!(
            event["type"].is_string(),
            "records[{index}].event.type names the kind"
        );
        assert!(event["seq"].is_i64(), "records[{index}].event.seq");
        assert!(event["time"].is_i64(), "records[{index}].event.time");
        assert!(event["data"].is_object(), "records[{index}].event.data");
    }
}

#[test]
fn a_translated_record_is_wrapped_in_the_same_envelope() {
    // The envelope must add exactly one level and change nothing inside it: the
    // flat event `record_to_event` produces, under `event`.
    let records = [
        record(12, 1_792_175_537_323, 1, RecordKind::TurnStart, None),
        record(
            17,
            1_792_175_537_386,
            1,
            RecordKind::UserMessage,
            Some("smoke test"),
        ),
        record(24, 1_792_175_537_394, 1, RecordKind::TurnEnd, Some("error")),
    ];
    for record in &records {
        let envelope = record_to_envelope(record).expect("this kind is sent");
        assert_eq!(envelope["type"], "event");
        let event = &envelope["event"];
        let flat = record_to_event(record).expect("the flat shape exists too");
        assert_eq!(event, &flat, "the envelope wraps the translation unchanged");
        assert!(event["type"].is_string());
        assert_eq!(event["seq"], record.seq);
        assert_eq!(event["time"], record.time);
        assert!(event["data"].is_object());
    }
}

#[test]
fn turn_start_and_turn_end_match_the_golden_data() {
    // The first and last elements of the fixture, rebuilt from the shared model:
    // same envelope, same event type, seq, time and data.
    let fixture = golden();
    let first = &fixture["result"]["records"][0];
    let turn_start = record_to_envelope(&record(
        12,
        1_792_175_537_323,
        1,
        RecordKind::TurnStart,
        None,
    ))
    .expect("turn/start is sent");
    assert_eq!(turn_start["type"], first["type"]);
    assert_eq!(turn_start["event"]["type"], first["event"]["type"]);
    assert_eq!(turn_start["event"]["seq"], first["event"]["seq"]);
    assert_eq!(turn_start["event"]["time"], first["event"]["time"]);
    assert_eq!(turn_start["event"]["data"], first["event"]["data"]);

    let last = &fixture["result"]["records"][3];
    let turn_end = record_to_envelope(&record(
        24,
        1_792_175_537_394,
        1,
        RecordKind::TurnEnd,
        Some("error"),
    ))
    .expect("turn/end is sent");
    assert_eq!(turn_end["type"], last["type"]);
    assert_eq!(turn_end["event"]["type"], last["event"]["type"]);
    assert_eq!(turn_end["event"]["seq"], last["event"]["seq"]);
    assert_eq!(
        turn_end["event"]["data"]["turn"],
        last["event"]["data"]["turn"]
    );
    // A failed turn says so; the fixture's last element is one.
    assert_eq!(
        turn_end["event"]["data"]["reason"]["kind"],
        last["event"]["data"]["reason"]["kind"]
    );
}

#[test]
fn the_answer_frame_wraps_the_envelopes_like_the_golden_frame() {
    // The envelopes travel inside `ServerFrame::ok`: v/type/id/result, with the
    // fixture's own `v`, `type` and `id`.
    let result = json!({
        "sessionId": "session-edd15eac-6549-4d6a-99d5-fa9d8ad74fc6",
        "asOfSeq": 12,
        "records": [
            record_to_envelope(&record(12, 1_792_175_537_323, 1, RecordKind::TurnStart, None))
                .expect("turn/start is sent"),
        ],
        "hasMore": false,
    });
    let frame = serde_json::to_value(ServerFrame::ok("w9", result)).expect("the frame serializes");
    let fixture = golden();
    assert_eq!(frame["v"], fixture["v"]);
    assert_eq!(frame["type"], fixture["type"]);
    assert_eq!(frame["id"], fixture["id"]);
    assert_eq!(frame["result"]["records"][0]["type"], "event");
    assert_eq!(frame["result"]["records"][0]["event"]["type"], "turn/start");
}

#[test]
fn tool_records_have_no_envelope_at_all() {
    // A tool call is not part of the summary, and the envelope must not invent a
    // placeholder for it.
    let tool = Record {
        seq: 20,
        time: 0,
        turn: Some(1),
        step: Some(1),
        kind: RecordKind::ToolCall,
        text: None,
        tool: None,
    };
    assert!(record_to_event(&tool).is_none());
    assert!(record_to_envelope(&tool).is_none());
}
