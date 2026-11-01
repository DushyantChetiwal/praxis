//! A direct phone link, preferred over the relays while it is open. The
//! relays only carry the offer and answer that set it up. The transport is
//! supplied by the application, so this crate does not build WebRTC.

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use gpui::{App, Global};

pub trait DirectConnection: Send + Sync {
    /// Whether a message sent now would go over the direct link.
    fn is_open(&self) -> bool;
    /// Sends one message, or returns false when the link cannot take it.
    fn send(&self, message: &[u8]) -> bool;
}

/// Called with each message the phone sends over the direct link.
pub type OnDirectMessage = Box<dyn Fn(Vec<u8>) + Send + Sync>;

pub trait DirectTransport: Send + Sync {
    /// Answers a phone's session offer. Resolves with the connection and the
    /// session answer to send back over the relay.
    fn accept(
        &self,
        offer: String,
        on_message: OnDirectMessage,
    ) -> BoxFuture<'static, Result<(Arc<dyn DirectConnection>, String)>>;
}

#[derive(Clone)]
struct GlobalDirectTransport(Arc<dyn DirectTransport>);

impl Global for GlobalDirectTransport {}

pub fn set_direct_transport(transport: Arc<dyn DirectTransport>, cx: &mut App) {
    cx.set_global(GlobalDirectTransport(transport));
}

pub(super) fn direct_transport(cx: &App) -> Option<Arc<dyn DirectTransport>> {
    cx.try_global::<GlobalDirectTransport>()
        .map(|global| global.0.clone())
}
