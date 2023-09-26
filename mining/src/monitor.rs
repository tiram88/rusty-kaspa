use super::MiningCounters;
use kaspa_core::{
    info,
    task::{
        service::{AsyncService, AsyncServiceFuture},
        tick::{TickReason, TickService},
    },
    trace,
};
use kaspa_txscript::caches::TxScriptCacheCounters;
use std::{sync::Arc, time::Duration};

const MONITOR: &str = "mempool-monitor";

pub struct MiningMonitor {
    // Counters
    counters: Arc<MiningCounters>,

    tx_script_cache_counters: Arc<TxScriptCacheCounters>,

    // Tick service
    tick_service: Arc<TickService>,
}

impl MiningMonitor {
    pub fn new(
        counters: Arc<MiningCounters>,
        tx_script_cache_counters: Arc<TxScriptCacheCounters>,
        tick_service: Arc<TickService>,
    ) -> MiningMonitor {
        MiningMonitor { counters, tx_script_cache_counters, tick_service }
    }

    pub async fn worker(self: &Arc<MiningMonitor>) {
        let mut last_snapshot = self.counters.snapshot();
        let mut last_tx_script_cache_snapshot = self.tx_script_cache_counters.snapshot();
        let snapshot_interval = 10;
        loop {
            if let TickReason::Shutdown = self.tick_service.tick(Duration::from_secs(snapshot_interval)).await {
                // Let the system print final logs before exiting
                tokio::time::sleep(Duration::from_millis(500)).await;
                break;
            }

            let snapshot = self.counters.snapshot();
            let tx_script_cache_snapshot = self.tx_script_cache_counters.snapshot();
            if snapshot == last_snapshot {
                // No update, avoid printing useless info
                continue;
            }

            // Subtract the snapshots
            let delta = &snapshot - &last_snapshot;
            let tx_script_cache_delta = &tx_script_cache_snapshot - &last_tx_script_cache_snapshot;

            info!(
                "Tx throughput stats: {:.2} u-tps, {:.2}% e-tps (in: {} via RPC, {} via P2P, out: {} via accepted blocks)",
                delta.u_tps(),
                delta.e_tps() * 100.0,
                delta.high_priority_tx_counts,
                delta.low_priority_tx_counts,
                delta.tx_accepted_counts,
            );
            // FIXME: (wip) decide if the log level should be debug and what info should be kept or formulated differently
            if tx_script_cache_snapshot != last_tx_script_cache_snapshot {
                info!(
                    "UTXOs stats: {} spent, {} created ({} signatures validated, {} cache hits, {:.2} hit ratio)",
                    delta.input_counts,
                    delta.output_counts,
                    tx_script_cache_delta.insert_counts,
                    tx_script_cache_delta.get_counts,
                    tx_script_cache_delta.hit_ratio()
                );
            }

            last_snapshot = snapshot;
            last_tx_script_cache_snapshot = tx_script_cache_snapshot;
        }

        trace!("mempool monitor thread exiting");
    }
}

// service trait implementation for Monitor
impl AsyncService for MiningMonitor {
    fn ident(self: Arc<Self>) -> &'static str {
        MONITOR
    }

    fn start(self: Arc<Self>) -> AsyncServiceFuture {
        Box::pin(async move {
            self.worker().await;
            Ok(())
        })
    }

    fn signal_exit(self: Arc<Self>) {
        trace!("sending an exit signal to {}", MONITOR);
    }

    fn stop(self: Arc<Self>) -> AsyncServiceFuture {
        Box::pin(async move {
            trace!("{} stopped", MONITOR);
            Ok(())
        })
    }
}
