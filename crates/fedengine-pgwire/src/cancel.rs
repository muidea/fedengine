use modorch::prelude::async_trait;
use pgwire::{api::cancel::CancelHandler, messages::cancel::CancelRequest};
use std::{collections::BTreeMap, sync::Mutex};
use tokio_util::sync::CancellationToken;
pub(super) type CancelKey = (i32, Vec<u8>);
#[derive(Default)]
pub(super) struct Cancellations(pub(super) Mutex<BTreeMap<CancelKey, CancellationToken>>);
#[async_trait]
impl CancelHandler for Cancellations {
    async fn on_cancel_request(&self, r: CancelRequest) {
        if let Some(c) = self
            .0
            .lock()
            .unwrap()
            .get(&(r.pid, r.secret_key.to_bytes().to_vec()))
        {
            c.cancel();
        }
    }
}
