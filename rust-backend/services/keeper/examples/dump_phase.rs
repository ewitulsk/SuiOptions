//! Verification harness for the genesis-`Settling` crank loop (SO bug:
//! keeper spams `select_bucket` → abort 35 `vault_wrong_phase`).
//!
//! Reads a live vault exactly the way the tick loop does
//! (`obj.fields.to_json_value()`), prints the raw `phase` encoding, then
//! runs the real `parse_vault_view` + `plan` and prints what the keeper
//! would do. Confirms whether `view.settling` is parsed correctly.
//!
//! Run: `cargo run -p keeper --example dump_phase -- [VAULT_ID]`
//! (defaults to the two genesis-stuck staging vaults).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use sui_json_rpc_types::{SuiObjectDataOptions, SuiParsedData};
use sui_sdk::SuiClientBuilder;
use sui_types::base_types::ObjectID;

use keeper::planner::{plan, PlanInput};
use keeper::state::parse_vault_view;

const RPC: &str = "https://fullnode.testnet.sui.io:443";
const DEFAULT_VAULTS: &[&str] = &[
    "0x92a21fa326f6c122b47ab73c5204cf3d789cea56adc732b40659c65351e48a47",
    "0x7e4c621ce03395477ad2150136aeb2199c87cf73670bed67d9b0ab61632825f3",
];

#[tokio::main]
async fn main() -> Result<()> {
    let client = SuiClientBuilder::default().build(RPC).await?;

    let ids: Vec<String> = {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.is_empty() {
            DEFAULT_VAULTS.iter().map(|s| s.to_string()).collect()
        } else {
            args
        }
    };

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;

    for id_str in ids {
        let id: ObjectID = id_str.parse()?;
        println!("\n════════ vault {id} ════════");

        let resp = client
            .read_api()
            .get_object_with_options(id, SuiObjectDataOptions::new().with_content())
            .await?;
        let data = resp.data.ok_or_else(|| anyhow!("vault {id} not found"))?;
        let move_struct = match data.content {
            Some(SuiParsedData::MoveObject(obj)) => obj.fields,
            other => return Err(anyhow!("unexpected content: {other:?}")),
        };

        // Compare three representations of the `phase` enum field.
        let serde_fields = serde_json::to_value(&move_struct)?;
        println!("typed field_value(phase): {:?}", move_struct.field_value("phase"));
        println!("serde phase JSON:        {}", serde_fields.get("phase").unwrap_or(&serde_json::Value::Null));

        let fields = move_struct.to_json_value();
        // The exact shape the parser currently sees for `phase`.
        println!("to_json_value phase:     {}", fields.get("phase").unwrap_or(&serde_json::Value::Null));

        let view = parse_vault_view(&fields)?;
        println!(
            "parsed: settling={} round={} current_bucket={:?} current_expiry_ms={} open_rfqs={} open_swap_rfqs={}",
            view.settling,
            view.round,
            view.current_bucket,
            view.current_expiry_ms,
            view.open_rfqs,
            view.open_swap_rfqs,
        );

        let action = plan(&PlanInput {
            view: &view,
            now_ms: now,
            auctions: &[],
            swap_auctions: &[],
            bucket_meta: None,
            stagger_ms: 90 * 60_000,
            max_slices: 4,
        });
        println!("plan() => {action:?}");
    }

    Ok(())
}
