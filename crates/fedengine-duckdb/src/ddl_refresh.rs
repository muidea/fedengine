//! Wait for publication only after the committing worker has released its leases.
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use std::{sync::atomic::Ordering, time::Duration};

pub(super) async fn refresh(
    hub: &modorch::EventHub,
    storage: &ManagementStorage,
    registry: &DataSourceRegistry,
    source: &DataSourceId,
    timeout: Duration,
) -> bool {
    let Ok(ticket) = hub.request::<_, Result<ManagementReply>>(
        MANAGEMENT_TOPIC,
        modorch::Destination::Component(modorch::ComponentRef::module("source-manager")),
        ManagementRequest::DdlInvalidated(source.clone()),
        Default::default(),
    ) else {
        return false;
    };
    // A timeout stops waiting, never cancels or replays the committed remote DDL.
    tokio::time::timeout(timeout, async {
        let Ok(reply) = ticket.wait(timeout).await else {
            return false;
        };
        if reply.as_ref().is_err() {
            return false;
        }
        loop {
            // Read small effective heads, not the entire catalog payload on every poll.
            let Ok(heads) = storage.port.catalog_headers().await else {
                return false;
            };
            let Some(head) = heads.sources.iter().find(|h| &h.source.id == source) else {
                return false;
            };
            if head.source.current_operation.is_none() {
                return head.source.status == SourceStatus::Active
                    && head.snapshot.as_ref().is_some_and(|s| !s.stale)
                    && registry.get(source).is_ok_and(|s| {
                        !s.stale.load(Ordering::SeqCst) && s.admitted.load(Ordering::SeqCst)
                    });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}
