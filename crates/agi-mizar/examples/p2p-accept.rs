//! The Rust half of the cross-language direct-path test.
//!
//! Binds an engine, prints the endpoint id and port the Swift probe needs, then
//! accepts one stream, prints what it received, and echoes it back. The Swift
//! half lives in `iOS/MizarKit/Probe/main.swift`; `scripts/p2p-interop.sh`
//! starts both and asserts the exchange.
//!
//! Deliberately built on the public engine API, for the same reason the probe
//! is: an engine that cannot be driven from another module is not an engine an
//! app can rely on.

use std::time::Duration;

use agi_mizar::net::engine::{Engine, Reach};
use agi_mizar::net::identity::{Identity, Purpose};

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("the secret must have an even number of hex digits".to_string());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16).map_err(|error| error.to_string())
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secret = match std::env::args().nth(1) {
        Some(hex) => decode_hex(&hex)?,
        None => vec![0x42; 32],
    };
    if secret.len() != 32 {
        return Err("the secret must be exactly 32 bytes".into());
    }

    let identity = Identity::derive(&secret, Purpose::Host);
    let reach = Reach::default();
    let (mut engine, address) = Engine::bind(identity, 0, reach, Vec::new()).await?;
    println!("ID {}", engine.endpoint_id());
    println!("PORT {}", address.port());

    let mut stream = tokio::time::timeout(Duration::from_secs(20), engine.accept())
        .await
        .expect("the probe should dial within 20 s")
        .expect("the engine should accept a stream");
    let message = tokio::time::timeout(Duration::from_secs(20), stream.receive())
        .await
        .expect("the probe should send within 20 s")
        .expect("the stream should carry a message");
    let text = String::from_utf8_lossy(&message);
    println!("RECEIVED {text}");
    stream.send(format!("echo:{text}").into_bytes()).await?;
    // Let the queued datagram leave before the process ends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}
