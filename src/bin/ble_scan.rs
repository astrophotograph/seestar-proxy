//! Seestar BLE protocol explorer.
//!
//! Scans for Seestar devices, connects to the first one found, and prints
//! every GATT service and characteristic — including raw values for readable
//! characteristics.  Run this near the telescope to discover the UUIDs and
//! data formats needed for the home-network pairing flow.
//!
//! Build and run:
//!   cargo run --bin ble-scan --features bluetooth
//!
//! Optional: filter by device name prefix (default: "S50_")
//!   cargo run --bin ble-scan --features bluetooth -- --name S50_4ddb0535

use btleplug::api::{
    Central, CentralEvent, CharPropFlags, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Manager, Peripheral};
use clap::Parser;
use futures_util::StreamExt;
use std::time::Duration;
use tokio::time::timeout;

#[derive(Parser)]
#[command(about = "Scan for Seestar BLE devices and dump their GATT profile")]
struct Args {
    /// Device name prefix to match (case-insensitive)
    #[arg(long, default_value = "S50_")]
    name: String,

    /// How long to scan before giving up (seconds)
    #[arg(long, default_value_t = 15)]
    scan_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Bluetooth adapter found"))?;

    println!("Adapter: {:?}", adapter.adapter_info().await?);
    println!("Scanning for devices matching {:?} ...", args.name);

    adapter.start_scan(ScanFilter::default()).await?;

    let mut events = adapter.events().await?;
    let deadline = Duration::from_secs(args.scan_secs);
    let name_lower = args.name.to_lowercase();

    let peripheral: Peripheral = timeout(deadline, async {
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
                let device_name = props.local_name.as_deref().unwrap_or("").to_lowercase();
                if device_name.contains(&name_lower) {
                    println!(
                        "\nFound: {} ({:?})",
                        props.local_name.as_deref().unwrap_or("(unnamed)"),
                        p.id()
                    );
                    if !props.service_data.is_empty() {
                        println!("  Service data: {:?}", props.service_data);
                    }
                    if !props.manufacturer_data.is_empty() {
                        println!("  Manufacturer data: {:?}", props.manufacturer_data);
                    }
                    if !props.services.is_empty() {
                        println!("  Advertised services: {:?}", props.services);
                    }
                    return Ok(p);
                }
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Scan timed out after {}s — is the telescope on?",
            args.scan_secs
        )
    })??;

    adapter.stop_scan().await?;

    println!("\nConnecting...");
    peripheral.connect().await?;
    peripheral.discover_services().await?;

    let services = peripheral.services();
    println!("\n=== GATT Profile ===\n");

    for service in &services {
        println!("Service: {}", service.uuid);
        println!("  Primary: {}", service.primary);

        for characteristic in &service.characteristics {
            let props = &characteristic.properties;
            let flags = format_flags(props);

            println!("  Characteristic: {}", characteristic.uuid);
            println!("    Properties: {}", flags);

            if props.contains(CharPropFlags::READ) {
                match peripheral.read(&characteristic).await {
                    Ok(data) => {
                        println!("    Value (raw):  {:?}", data);
                        if let Ok(s) = std::str::from_utf8(&data) {
                            println!("    Value (utf8): {:?}", s);
                        }
                    }
                    Err(e) => println!("    Read error: {}", e),
                }
            }

            for descriptor in &characteristic.descriptors {
                println!("    Descriptor: {}", descriptor.uuid);
            }
        }
        println!();
    }

    println!("Disconnecting...");
    peripheral.disconnect().await?;

    Ok(())
}

fn format_flags(flags: &CharPropFlags) -> String {
    let mut parts = Vec::new();
    if flags.contains(CharPropFlags::READ) {
        parts.push("READ");
    }
    if flags.contains(CharPropFlags::WRITE) {
        parts.push("WRITE");
    }
    if flags.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) {
        parts.push("WRITE_NO_RSP");
    }
    if flags.contains(CharPropFlags::NOTIFY) {
        parts.push("NOTIFY");
    }
    if flags.contains(CharPropFlags::INDICATE) {
        parts.push("INDICATE");
    }
    if flags.contains(CharPropFlags::BROADCAST) {
        parts.push("BROADCAST");
    }
    parts.join(" | ")
}
