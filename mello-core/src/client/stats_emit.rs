use crate::stats::{self, MelloStats};
use crate::Client;

impl Client {
    pub fn collect_stats(&self) -> MelloStats {
        let rss_bytes = stats::process_rss_bytes().unwrap_or(0);
        let footprint_bytes = stats::proc_rusage(std::process::id())
            .map(|r| r.phys_footprint_bytes)
            .unwrap_or(0);
        MelloStats {
            nakama_connected: self.nakama.is_ws_connected(),
            voice_active: self.voice.is_active(),
            stream_hosting: self.stream_session.is_some(),
            stream_watching: self.viewer_state.is_some(),
            process_rss_mb: stats::rss_to_mb(rss_bytes),
            process_footprint_mb: stats::rss_to_mb(footprint_bytes),
        }
    }

    pub(super) fn emit_stats_tick(&self) {
        if !self.emit_process_stats {
            return;
        }
        let snapshot = self.collect_stats();
        let _ = self
            .event_tx
            .send(crate::events::Event::StatsUpdated { stats: snapshot });
    }
}
