//! Recording every JSON-RPC frame as it crosses the transport.
//!
//! The frames are the only account of a session that is not an interpretation
//! of one. When a tool call fails for a reason the rendered result does not
//! explain — a coerced argument, a `_meta` field, an error body the client
//! folds into a generic message — the exchange itself is the answer, and no
//! amount of UI above this layer can reconstruct it after the fact.
//!
//! Wrapping the transport rather than the peer is what makes that complete:
//! `initialize` and the handshake happen before any `Handle` exists, and a
//! notification the client does not model still shows up here as bytes.
//!
//! Nothing is masked. This layer only ever sees JSON-RPC bodies — OAuth tokens
//! and custom headers ride the HTTP layer above it and never reach a frame —
//! so there is no secret here to redact, and a redaction that hid a real
//! argument would defeat the point of having the transcript at all.

use std::future::Future;

use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::broadcast;

use crate::session::Event;

pub(crate) struct Recording<T> {
    inner: T,
    events: broadcast::Sender<Event>,
}

impl<T> Recording<T> {
    pub(crate) fn new(inner: T, events: broadcast::Sender<Event>) -> Self {
        Self { inner, events }
    }
}

/// A frame that will not serialize is dropped rather than reported: the
/// transcript is diagnostic, and failing a live connection to preserve a log
/// line would be the wrong trade.
fn emit(events: &broadcast::Sender<Event>, outbound: bool, message: &impl serde::Serialize) {
    if let Ok(message) = serde_json::to_value(message) {
        let _ = events.send(Event::Wire { outbound, message });
    }
}

impl<T> Transport<RoleClient> for Recording<T>
where
    T: Transport<RoleClient>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Recorded before the send rather than after it: a frame that fails to
        // go out is exactly the one worth having in the transcript.
        emit(&self.events, true, &item);
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        // Split the borrow: the returned future holds `inner` mutably, so the
        // sender has to be cloned out before it rather than read through
        // `self` inside.
        let events = self.events.clone();
        let inner = &mut self.inner;
        async move {
            let message = inner.receive().await;
            if let Some(message) = &message {
                emit(&events, false, message);
            }
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}
