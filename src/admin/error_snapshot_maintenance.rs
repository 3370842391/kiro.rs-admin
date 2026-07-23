use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::error_snapshot_db::{MaintenanceReport, SharedErrorSnapshotStore};
use super::trace_db::SharedTraceStore;

pub async fn run_blocking<F, T>(job: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(job).await
}

pub async fn run_maintenance_batch(
    store: SharedErrorSnapshotStore,
    trace_store: Option<SharedTraceStore>,
) -> anyhow::Result<MaintenanceReport> {
    run_blocking(move || {
        let report = store.run_maintenance()?;
        if let Some(trace_store) = trace_store {
            for (trace_id, snapshot_id) in
                store.recent_trace_links(chrono::Utc::now().timestamp() - 7 * 86_400)?
            {
                trace_store.link_snapshot(&trace_id, &snapshot_id);
            }
        }
        Ok::<_, anyhow::Error>(report)
    })
    .await
    .map_err(|error| anyhow::anyhow!("错误快照维护线程异常结束: {error}"))?
}

pub fn spawn_scheduler(
    store: SharedErrorSnapshotStore,
    trace_store: Option<SharedTraceStore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let running = Arc::new(AtomicBool::new(false));
        tokio::time::sleep(Duration::from_secs(1)).await;
        loop {
            if running.swap(true, Ordering::AcqRel) {
                tokio::task::yield_now().await;
                continue;
            }
            let result = run_maintenance_batch(store.clone(), trace_store.clone()).await;
            running.store(false, Ordering::Release);
            let delay = match result {
                Ok(report) => {
                    if report.deleted > 0 || report.imported > 0 {
                        tracing::info!(
                            deleted = report.deleted,
                            imported = report.imported,
                            needs_follow_up = report.needs_follow_up,
                            "错误快照有界维护完成"
                        );
                    }
                    if report.needs_follow_up {
                        Duration::from_millis(250)
                    } else {
                        Duration::from_secs(3600)
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "错误快照维护失败");
                    Duration::from_secs(3600)
                }
            };
            tokio::time::sleep(delay).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_maintenance_does_not_stall_runtime_heartbeat() {
        let maintenance = tokio::spawn(run_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            7_u8
        }));

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            tokio::time::sleep(std::time::Duration::from_millis(10)),
        )
        .await
        .expect("runtime heartbeat must stay responsive");
        assert!(!maintenance.is_finished());
        assert_eq!(maintenance.await.unwrap().unwrap(), 7);
    }
}
