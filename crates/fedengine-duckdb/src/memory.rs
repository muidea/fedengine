//! Phase observations supplement native engine budgets, which do not bound RSS.
use std::time::Instant;

#[derive(Default)]
struct Resident {
    rss_kib: Option<u64>,
    peak_kib: Option<u64>,
}
impl Resident {
    fn read() -> Self {
        let mut resident = Self::default();
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.split_whitespace().next().and_then(|v| v.parse().ok());
                match name {
                    "VmRSS" => resident.rss_kib = value,
                    "VmHWM" => resident.peak_kib = value,
                    _ => {}
                }
            }
        }
        resident
    }
}
pub(super) struct Stage {
    name: &'static str,
    started: Instant,
    before: Resident,
}
impl Stage {
    pub(super) fn new(name: &'static str) -> Self {
        let before = Resident::read();
        tracing::debug!(
            phase = name,
            rss_kib = before.rss_kib,
            peak_rss_kib = before.peak_kib,
            "metadata phase started"
        );
        Self {
            name,
            started: Instant::now(),
            before,
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let after = Resident::read();
        let delta = after
            .rss_kib
            .zip(self.before.rss_kib)
            .map(|(a, b)| a as i64 - b as i64);
        tracing::debug!(
            phase = self.name,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.,
            rss_kib = after.rss_kib,
            peak_rss_kib = after.peak_kib,
            rss_delta_kib = delta,
            "metadata phase finished"
        );
    }
}
