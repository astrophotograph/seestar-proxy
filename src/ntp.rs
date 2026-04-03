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

#[cfg(test)]
mod tests {
    use super::*;

    /// sync() against localhost returns an error immediately (no NTP server at 127.0.0.1:123).
    /// This covers the error path through `client.synchronize()`.
    #[test]
    fn sync_returns_error_when_server_unreachable() {
        let result = sync("127.0.0.1");
        assert!(result.is_err(), "sync must fail when no NTP server is running");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("NTP query failed"),
            "error must say 'NTP query failed', got: {}",
            msg
        );
    }

    /// Exercises the error path inside set_system_clock on unix (requires root / CAP_SYS_TIME).
    /// In the test environment we're not root, so clock_settime returns EPERM.
    #[cfg(unix)]
    #[test]
    fn set_system_clock_fails_without_root_privileges() {
        // Use a known-valid time (year 2024) — the call will fail with EPERM, not an argument error.
        let duration = std::time::Duration::from_secs(1_700_000_000);
        let result = set_system_clock(duration);
        // If we happen to be root (e.g. Docker), this might succeed — that's also fine.
        if result.is_err() {
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("clock_settime failed"),
                "error must mention clock_settime, got: {}",
                msg
            );
        }
    }

    #[test]
    fn drift_threshold_is_positive() {
        assert!(DRIFT_THRESHOLD_SECS > 0.0);
    }
}
