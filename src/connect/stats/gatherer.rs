use parking_lot::RwLock;
use smol_str::SmolStr;
use sosistab2::PipeStats;

use std::time::SystemTime;

use crate::debugpack::DEBUGPACK;

#[derive(Clone, Debug)]
pub struct StatItem {
    pub time: SystemTime,
    pub endpoint: SmolStr,
    pub protocol: SmolStr,
    pub stats: PipeStats,
    pub send_bytes: u64,
    pub recv_bytes: u64,
}

#[derive(Default)]
pub struct StatsGatherer {
    buffer: RwLock<im::Vector<StatItem>>,
}

impl StatsGatherer {
    /// Pushes a stat item to the gatherer.
    pub fn push(&self, item: StatItem) {
        let _ = DEBUGPACK.add_timeseries("send_mb", item.send_bytes as f64 / 1_000_000.0);
        let _ = DEBUGPACK.add_timeseries("recv_mb", item.recv_bytes as f64 / 1_000_000.0);
        let _ = DEBUGPACK.add_timeseries("latency_ms", item.stats.latency.as_secs_f64() * 1000.0);
        let _ = DEBUGPACK.add_timeseries("loss", item.stats.loss);
        let mut buffer = self.buffer.write();
        buffer.push_back(item);
        if buffer.len() > 10000 {
            buffer.pop_front();
        }
    }

    /// Obtains all the stats items.
    pub fn all_items(&self) -> im::Vector<StatItem> {
        self.buffer.read().clone()
    }
}
