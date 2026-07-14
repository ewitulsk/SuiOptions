//! Cross-checks the hand-written event mirrors in `src/events.rs` against
//! the committed Anchor IDL snapshots (`tests/fixtures/*.json`).
//!
//! Borsh is positional, so a silent field reorder in the programs would
//! mis-decode without erroring. This test synthesizes a Borsh buffer per
//! IDL event — every field gets a position-distinct value — decodes it
//! through the real registry, and asserts each field surfaces in the JSON
//! payload under the IDL's name with the expected wire encoding. Any
//! drift in names, order, types, or discriminators fails here.
//!
//! When the programs' events change: regenerate the IDLs (`anchor build`
//! in solana-contracts/), copy them over `tests/fixtures/`, and update
//! `src/events.rs` to match.

use serde_json::Value;
use solana_indexer::events::{decode_event, event_discriminator, Program};

const FIXTURES: &[(&str, Program)] = &[
    ("options_core", Program::Core),
    ("auction_venue", Program::Venue),
    ("options_vault", Program::Vault),
];

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture readable"))
        .expect("fixture parses")
}

/// Encode one IDL field with a position-distinct marker value; return the
/// expected JSON form the payload must carry for it.
fn encode_field(buf: &mut Vec<u8>, field_ty: &Value, n: u8) -> Value {
    match field_ty {
        Value::String(s) => match s.as_str() {
            "pubkey" => {
                buf.extend_from_slice(&[n; 32]);
                Value::String(bs58::encode([n; 32]).into_string())
            }
            "u64" => {
                buf.extend_from_slice(&(n as u64).to_le_bytes());
                Value::String(n.to_string())
            }
            "u128" => {
                buf.extend_from_slice(&(n as u128).to_le_bytes());
                Value::String(n.to_string())
            }
            "u8" => {
                buf.push(n);
                Value::Number(n.into())
            }
            "bool" => {
                let b = n % 2 == 1;
                buf.push(b as u8);
                Value::Bool(b)
            }
            "string" => {
                let s = format!("s{n}");
                buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
                Value::String(s)
            }
            "bytes" => {
                buf.extend_from_slice(&3u32.to_le_bytes());
                buf.extend_from_slice(&[n; 3]);
                Value::String(format!("{n:02x}{n:02x}{n:02x}"))
            }
            other => panic!("unhandled IDL primitive {other:?}"),
        },
        Value::Object(o) if o.contains_key("option") => {
            buf.push(1); // Some
            encode_field(buf, &o["option"], n)
        }
        Value::Object(o) if o.contains_key("defined") => {
            let name = o["defined"]["name"].as_str().unwrap();
            assert_eq!(
                name, "AuctionMode",
                "only AuctionMode is mirrored as a defined type"
            );
            let variant = n % 3;
            buf.push(variant);
            Value::String(
                ["swap", "covered_call", "cash_secured_put"][variant as usize].to_string(),
            )
        }
        other => panic!("unhandled IDL field type {other:?}"),
    }
}

#[test]
fn every_idl_event_round_trips_through_the_mirrors() {
    for (fixture_name, program) in FIXTURES {
        let idl = fixture(fixture_name);
        let events = idl["events"].as_array().expect("events array");
        assert!(!events.is_empty());
        let types: std::collections::HashMap<&str, &Value> = idl["types"]
            .as_array()
            .expect("types array")
            .iter()
            .map(|t| (t["name"].as_str().unwrap(), &t["type"]))
            .collect();

        for event in events {
            let name = event["name"].as_str().unwrap();

            // 1. Discriminator: IDL bytes == sha256("event:{name}")[..8].
            let idl_disc: Vec<u8> = event["discriminator"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            assert_eq!(
                idl_disc,
                event_discriminator(name).to_vec(),
                "{fixture_name}::{name}: discriminator mismatch"
            );

            // 2. Layout: synthesize Borsh per the IDL, decode via the
            //    registry, compare every payload field.
            let fields = types[name]["fields"].as_array().unwrap();
            let mut buf = event_discriminator(name).to_vec();
            let mut expected = Vec::new();
            for (i, field) in fields.iter().enumerate() {
                let fname = field["name"].as_str().unwrap();
                let fval = encode_field(&mut buf, &field["type"], (i as u8) + 1);
                expected.push((fname.to_string(), fval));
            }

            let decoded = decode_event(*program, &buf)
                .unwrap_or_else(|e| panic!("{fixture_name}::{name}: decode failed: {e}"))
                .unwrap_or_else(|| panic!("{fixture_name}::{name}: no mirror registered"));
            assert_eq!(decoded.tag(), name);
            assert_eq!(decoded.program(), *program);

            let payload = decoded.payload().unwrap();
            let obj = payload.as_object().unwrap();
            assert_eq!(
                obj.len(),
                fields.len(),
                "{fixture_name}::{name}: payload field count != IDL"
            );
            for (fname, fval) in expected {
                assert_eq!(
                    obj.get(&fname),
                    Some(&fval),
                    "{fixture_name}::{name}: field {fname} mismatch (order/type drift?)"
                );
            }
        }
    }
}

#[test]
fn mirror_count_matches_the_idls_exactly() {
    let idl_total: usize = FIXTURES
        .iter()
        .map(|(f, _)| fixture(f)["events"].as_array().unwrap().len())
        .sum();
    assert_eq!(idl_total, 50, "IDL fixtures should carry all 50 events");
}
