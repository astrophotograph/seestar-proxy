//! NTP time synchronization for headless devices without a real-time clock.
//!
//! Queries an NTP server via plain UDP (no TLS — works even when the
//! system clock is so wrong that certificate validation fails) and sets
//! the system clock if the offset exceeds a threshold.

use tracing::info;

/// Maximum acceptable clock offset before we correct it.
const DRIFT_THRESHOLD_SECS: f64 = 5.0;

/// Query the NTP server and set the system clock if it's significantly off.
///
/// Returns the offset that was applied, or None if no correction was needed.
pub fn sync(server: &str) -> anyhow::Result<Option<f64>> {
    info!("NTP: querying {}...", server);

    let client = rsntp::SntpClient::new();
    let result = client
        .synchronize(server)
        .map_err(|e| anyhow::anyhow!("NTP query failed: {}", e))?;

    let offset_secs = result.clock_offset().as_secs_f64();
    let rtt_ms = result.round_trip_delay().as_secs_f64() * 1000.0;

    info!(
        "NTP: offset = {:.3}s, round-trip = {:.1}ms, stratum = {}",
        offset_secs,
        rtt_ms,
        result.stratum()
    );

    if offset_secs.abs() < DRIFT_THRESHOLD_SECS {
        info!(
            "NTP: clock is within {:.0}s threshold, no correction needed",
            DRIFT_THRESHOLD_SECS
        );
        return Ok(None);
    }

    // Get the correct time from the NTP response.
    let ntp_time = result
        .datetime()
        .into_system_time()
        .map_err(|e| anyhow::anyhow!("NTP: could not convert time: {}", e))?;

    let duration = ntp_time
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("NTP: time before epoch: {}", e))?;

    set_system_clock(duration)?;

    info!("NTP: system clock adjusted by {:.3}s", offset_secs);

    Ok(Some(offset_secs))
}

/// Set the system clock using libc::clock_settime (requires root or CAP_SYS_TIME).
#[cfg(unix)]
fn set_system_clock(since_epoch: std::time::Duration) -> anyhow::Result<()> {
    let ts = libc::timespec {
        tv_sec: since_epoch.as_secs() as libc::time_t,
        tv_nsec: since_epoch.subsec_nanos() as libc::c_long,
    };
    let ret = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("NTP: clock_settime failed: {} (are you root?)", err);
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_system_clock(_since_epoch: std::time::Duration) -> anyhow::Result<()> {
    anyhow::bail!("NTP: setting system clock is not supported on this platform")
}
