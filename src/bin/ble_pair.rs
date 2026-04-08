//! Seestar BLE WiFi pairing — full RSA auth + WiFi config.
//!
//! Mirrors the TCP auth flow from authenticate-rpi.lua over the BLE channel:
//!   1. get_verify_str (with sn token) → telescope sends a challenge
//!   2. RSA-SHA1 sign the challenge with client.pem
//!   3. verify_client {sign, data} → telescope confirms auth
//!   4. Send WiFi config (set_wifi_ssid / iscope_set_wifi)
//!
//! Build and run:
//!   cargo run --bin ble_pair --features bluetooth -- \
//!       --name S50_4ddb0535 --ssid MyNetwork --password MyPassword

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Manager, Peripheral};
use clap::Parser;
use futures_util::StreamExt;
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

const SERVICE_UUID: Uuid = Uuid::from_u128(0x850e1701_6ecf_49f1_a13f_f5c2d9174f9f);
const CHAR_UUID: Uuid = Uuid::from_u128(0x850e1702_6ecf_49f1_a13f_f5c2d9174f9f);

#[derive(Parser)]
#[command(about = "Authenticate over Seestar BLE and configure home WiFi")]
struct Args {
    /// Device name prefix to match
    #[arg(long, default_value = "S50_")]
    name: String,

    /// Client token stored on telescope (/data/zwo/app_data)
    #[arg(long, default_value = "rmtAXswcYEssidBo-VCzxZH0eI3UOppA")]
    sn: String,

    /// RSA private key (PEM) used to sign the auth challenge
    #[arg(long)]
    key: Option<String>,

    /// Home WiFi SSID to configure
    #[arg(long)]
    ssid: Option<String>,

    /// Home WiFi password to configure
    #[arg(long)]
    password: Option<String>,

    /// Scan timeout (seconds)
    #[arg(long, default_value_t = 15)]
    scan_secs: u64,

    /// How long to wait for each notification response (seconds)
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,

    /// How long to wait for the spontaneous challenge after subscribing (seconds).
    /// The telescope may need a few seconds to switch to AP mode before sending it.
    #[arg(long, default_value_t = 10)]
    challenge_wait_secs: u64,
}

/// Sign `challenge` with the RSA private key at `key_path` using RSA-SHA1
/// (PKCS#1 v1.5), return base64-encoded signature — same as authenticate-rpi.lua.
fn sign_challenge(key_path: &str, challenge: &str) -> anyhow::Result<String> {
    use std::io::Write;
    use std::process::Command;

    let tmp = std::env::temp_dir();
    let data_path = tmp.join("ble_challenge.bin");
    let sig_path = tmp.join("ble_challenge.sig");

    std::fs::File::create(&data_path)?.write_all(challenge.as_bytes())?;

    let status = Command::new("openssl")
        .args([
            "dgst",
            "-sha1",
            "-sign",
            key_path,
            "-out",
            sig_path.to_str().unwrap(),
            data_path.to_str().unwrap(),
        ])
        .status()?;
    anyhow::ensure!(status.success(), "openssl signing failed");

    let output = Command::new("base64")
        .args(["-w0", sig_path.to_str().unwrap()])
        .output()?;
    anyhow::ensure!(output.status.success(), "base64 encoding failed");

    let _ = std::fs::remove_file(&data_path);
    let _ = std::fs::remove_file(&sig_path);

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Write a JSON command and wait for the next notification, returning its parsed value.
async fn send_and_recv(
    peripheral: &Peripheral,
    characteristic: &btleplug::api::Characteristic,
    notifs: &mut (impl futures_util::Stream<Item = btleplug::api::ValueNotification> + Unpin),
    label: &str,
    msg: serde_json::Value,
    timeout_secs: u64,
) -> anyhow::Result<serde_json::Value> {
    let payload = serde_json::to_vec(&msg)?;
    println!("\n>> {} ({} bytes): {}", label, payload.len(), msg);
    peripheral
        .write(characteristic, &payload, WriteType::WithoutResponse)
        .await?;

    let resp = timeout(Duration::from_secs(timeout_secs), notifs.next())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for response to {}", label))?
        .ok_or_else(|| anyhow::anyhow!("notification stream ended"))?;

    let text = String::from_utf8_lossy(&resp.value);
    println!("<< {} bytes: {}", resp.value.len(), text);

    Ok(serde_json::from_slice(&resp.value)
        .unwrap_or_else(|_| serde_json::json!({"raw": text.to_string()})))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Bluetooth adapter found"))?;

    println!("Adapter: {:?}", adapter.adapter_info().await?);
    println!("Scanning for {:?} ...", args.name);
    adapter.start_scan(ScanFilter::default()).await?;

    let name_lower = args.name.to_lowercase();
    let mut events = adapter.events().await?;

    let peripheral: Peripheral = timeout(Duration::from_secs(args.scan_secs), async {
        loop {
            let Some(event) = events.next().await else {
                anyhow::bail!("Event stream ended");
            };
            if let CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) = event {
                let Ok(p) = adapter.peripheral(&id).await else {
                    continue;
                };
                let Ok(Some(props)) = p.properties().await else {
                    continue;
                };
                let name = props.local_name.as_deref().unwrap_or("").to_lowercase();
                if name.contains(&name_lower) {
                    println!("Found: {}", props.local_name.as_deref().unwrap_or("?"));
                    return Ok(p);
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Scan timed out — is the telescope on?"))??;

    adapter.stop_scan().await?;
    println!("Connecting...");
    peripheral.connect().await?;
    peripheral.discover_services().await?;

    let characteristic = peripheral
        .services()
        .iter()
        .find(|s| s.uuid == SERVICE_UUID)
        .and_then(|s| s.characteristics.iter().find(|c| c.uuid == CHAR_UUID))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Seestar characteristic not found"))?;

    // Get the notification stream BEFORE subscribing so we don't miss any
    // spontaneous messages the telescope sends immediately on connection.
    let mut notifs = peripheral.notifications().await?;
    peripheral.subscribe(&characteristic).await?;
    println!(
        "Subscribed to notifications. Waiting up to {}s for spontaneous challenge...",
        args.challenge_wait_secs
    );

    // The telescope switches to AP mode then sends a verification challenge
    // (send_air_verify_start) which may take several seconds to arrive.
    // Wait for the configured duration before falling back to get_verify_str.
    let spontaneous = timeout(Duration::from_secs(args.challenge_wait_secs), notifs.next()).await;

    let challenge: String = match spontaneous {
        Ok(Some(notif)) => {
            let text = String::from_utf8_lossy(&notif.value);
            println!("<< Spontaneous ({} bytes): {}", notif.value.len(), text);
            // Parse it as a challenge notification.
            let v: serde_json::Value = serde_json::from_slice(&notif.value)
                .unwrap_or_else(|_| serde_json::json!({"raw": text.to_string()}));
            v.pointer("/result/str")
                .or_else(|| v.get("result").filter(|r| r.is_string()))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    println!("(spontaneous message doesn't look like a challenge — falling back to get_verify_str)");
                    String::new()
                })
        }
        _ => {
            println!("(no spontaneous notification — will send get_verify_str)");
            String::new()
        }
    };

    // ── Step 1: get_verify_str (if telescope didn't send a challenge first) ───
    // BLE protocol uses "ble_method" and "ble_sn" instead of "method"/"sn".
    let challenge = if !challenge.is_empty() {
        challenge
    } else {
        let resp = send_and_recv(
            &peripheral,
            &characteristic,
            &mut notifs,
            "get_verify_str",
            serde_json::json!({
                "id": 1,
                "ble_method": "get_verify_str",
                "ble_sn": args.sn,
                "params": "verify"
            }),
            args.timeout_secs,
        )
        .await?;

        // An empty ack (no result field, no error) means the SN is already
        // trusted — no re-challenge needed.  Return a sentinel so callers
        // know to skip the sign+verify steps.
        if let Some(ch) = resp
            .pointer("/result/str")
            .or_else(|| resp.get("result").filter(|v| v.is_string()))
            .and_then(|v| v.as_str())
        {
            ch.to_string()
        } else if resp.get("code").is_none() && resp.get("error").is_none() {
            println!("(empty ack — SN already trusted, skipping verify)");
            String::from("__already_trusted__")
        } else {
            return Err(anyhow::anyhow!("No challenge in response: {}", resp));
        }
    };

    // ── Steps 2–3: sign + verify (skipped if SN is already trusted) ─────────────
    if challenge != "__already_trusted__" {
        println!("Challenge: {:?}", challenge);

        let key = args.key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("--key is required when the telescope issues a challenge")
        })?;
        let signature = sign_challenge(key, &challenge)?;
        println!(
            "Signature: {}...{}",
            &signature[..8],
            &signature[signature.len() - 8..]
        );

        let resp = send_and_recv(
            &peripheral,
            &characteristic,
            &mut notifs,
            "verify_client",
            serde_json::json!({
                "id": 2,
                "ble_method": "verify_client",
                "ble_sn": args.sn,
                "params": { "sign": signature, "data": challenge }
            }),
            args.timeout_secs,
        )
        .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        anyhow::ensure!(code == 0, "verify_client failed: {}", resp);
    }
    println!("Authenticated!");

    // ── Step 4: WiFi config ───────────────────────────────────────────────────
    if let (Some(ssid), Some(password)) = (&args.ssid, &args.password) {
        // Both methods accept the same params.  An empty ack (no error, no code)
        // is also treated as success — same response pattern as other accepted commands.
        for ble_method in &["iscope_set_wifi", "set_wifi_ssid"] {
            let resp = send_and_recv(
                &peripheral,
                &characteristic,
                &mut notifs,
                ble_method,
                serde_json::json!({
                    "id": 10,
                    "ble_method": ble_method,
                    "ble_sn": args.sn,
                    "params": { "ssid": ssid, "password": password }
                }),
                args.timeout_secs,
            )
            .await?;

            let code = resp.get("code").and_then(|v| v.as_i64());
            let error = resp.get("error");
            if code == Some(0) || (code.is_none() && error.is_none()) {
                println!("WiFi configured via {}!", ble_method);
                break;
            }
            println!("{} returned code {:?}: {}", ble_method, code, resp);
        }
    } else {
        // No WiFi args — show current WiFi state.
        let _ = send_and_recv(
            &peripheral,
            &characteristic,
            &mut notifs,
            "get_wifi_info",
            serde_json::json!({
                "id": 100,
                "ble_method": "get_wifi_info",
                "ble_sn": args.sn,
                "params": {}
            }),
            args.timeout_secs,
        )
        .await?;
    }

    println!("\nDisconnecting...");
    peripheral.disconnect().await?;
    Ok(())
}
