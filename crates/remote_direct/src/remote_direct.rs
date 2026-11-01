//! Direct links to the Praxis Remote phone app over a WebRTC data channel.
//! The phone offers; Praxis answers through the relay channel it already
//! trusts. Messages stay sealed with the paired key, so the link only
//! changes the route, never who can read them.

use gpui::App;

/// Lets Praxis Remote prefer a direct link where WebRTC is available.
#[cfg_attr(
    any(all(target_os = "windows", target_env = "gnu"), target_os = "freebsd"),
    allow(unused_variables)
)]
pub fn init(cx: &mut App) {
    #[cfg(not(any(all(target_os = "windows", target_env = "gnu"), target_os = "freebsd")))]
    agent_ui::set_direct_transport(std::sync::Arc::new(webrtc::WebRtcTransport::default()), cx);
}

#[cfg(not(any(all(target_os = "windows", target_env = "gnu"), target_os = "freebsd")))]
mod webrtc {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use agent_ui::{DirectConnection, DirectTransport, OnDirectMessage};
    use anyhow::{Context as _, Result, anyhow};
    use futures::FutureExt as _;
    use futures::channel::oneshot;
    use futures::future::BoxFuture;
    use libwebrtc::prelude::*;
    use parking_lot::Mutex;

    /// Free public servers that tell each side its public address. Without a
    /// relay server of our own, networks that block direct links keep using
    /// the Nostr relays.
    const STUN_SERVERS: [&str; 2] = [
        "stun:stun.l.google.com:19302",
        "stun:stun.cloudflare.com:3478",
    ];
    /// The answer carries every address found by then, so the phone needs no
    /// further exchange. Finding them rarely takes more than a second.
    const GATHERING_TIMEOUT: Duration = Duration::from_secs(3);

    #[derive(Default)]
    pub(crate) struct WebRtcTransport {
        // Started on the first offer: it runs its own threads.
        factory: OnceLock<PeerConnectionFactory>,
    }

    impl DirectTransport for WebRtcTransport {
        fn accept(
            &self,
            offer: String,
            on_message: OnDirectMessage,
        ) -> BoxFuture<'static, Result<(Arc<dyn DirectConnection>, String)>> {
            let factory = self.factory.get_or_init(PeerConnectionFactory::default).clone();
            async move {
                let connection = Arc::new(WebRtcConnection::new(&factory, on_message)?);
                let answer = connection.answer(&offer).await?;
                Ok((connection as Arc<dyn DirectConnection>, answer))
            }
            .boxed()
        }
    }

    pub(crate) struct WebRtcConnection {
        peer_connection: PeerConnection,
        channel: Arc<Mutex<Option<DataChannel>>>,
        open: Arc<AtomicBool>,
    }

    pub(crate) fn rtc_configuration(stun: bool) -> RtcConfiguration {
        RtcConfiguration {
            ice_servers: if stun {
                vec![IceServer {
                    urls: STUN_SERVERS.iter().map(|url| url.to_string()).collect(),
                    username: String::new(),
                    password: String::new(),
                }]
            } else {
                Vec::new()
            },
            continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
            ice_transport_type: IceTransportsType::All,
        }
    }

    impl WebRtcConnection {
        fn new(factory: &PeerConnectionFactory, on_message: OnDirectMessage) -> Result<Self> {
            Self::with_configuration(factory, rtc_configuration(true), on_message)
        }

        pub(crate) fn with_configuration(
            factory: &PeerConnectionFactory,
            configuration: RtcConfiguration,
            on_message: OnDirectMessage,
        ) -> Result<Self> {
            let peer_connection = factory.create_peer_connection(configuration)?;
            let channel = Arc::new(Mutex::new(None::<DataChannel>));
            let open = Arc::new(AtomicBool::new(false));
            let on_message = Arc::new(on_message);
            peer_connection.on_data_channel(Some(Box::new({
                let channel = channel.clone();
                let open = open.clone();
                move |data_channel: DataChannel| {
                    data_channel.on_message(Some(Box::new({
                        let on_message = on_message.clone();
                        move |buffer: DataBuffer| on_message(buffer.data.to_vec())
                    })));
                    data_channel.on_state_change(Some(Box::new({
                        let open = open.clone();
                        move |state: DataChannelState| {
                            open.store(state == DataChannelState::Open, Ordering::SeqCst)
                        }
                    })));
                    open.store(
                        data_channel.state() == DataChannelState::Open,
                        Ordering::SeqCst,
                    );
                    *channel.lock() = Some(data_channel);
                }
            })));
            Ok(Self {
                peer_connection,
                channel,
                open,
            })
        }

        pub(crate) async fn answer(&self, offer: &str) -> Result<String> {
            let offer = SessionDescription::parse(offer, SdpType::Offer)
                .map_err(|error| anyhow!("The phone's session offer is invalid: {error}"))?;
            let (gathered, gathered_signal) = oneshot::channel::<()>();
            let gathered = Arc::new(Mutex::new(Some(gathered)));
            let finish_gathering = {
                let gathered = gathered.clone();
                move || {
                    if let Some(gathered) = gathered.lock().take() {
                        gathered.send(()).ok();
                    }
                }
            };
            self.peer_connection
                .on_ice_gathering_state_change(Some(Box::new({
                    let finish_gathering = finish_gathering.clone();
                    move |state: IceGatheringState| {
                        if state == IceGatheringState::Complete {
                            finish_gathering();
                        }
                    }
                })));
            self.peer_connection.set_remote_description(offer).await?;
            let answer = self
                .peer_connection
                .create_answer(AnswerOptions::default())
                .await?;
            self.peer_connection.set_local_description(answer).await?;
            if self.peer_connection.ice_gathering_state() == IceGatheringState::Complete {
                finish_gathering();
            }
            std::thread::spawn({
                let finish_gathering = finish_gathering.clone();
                move || {
                    std::thread::sleep(GATHERING_TIMEOUT);
                    finish_gathering();
                }
            });
            gathered_signal.await.ok();
            let answer = self
                .peer_connection
                .current_local_description()
                .context("The direct connection produced no session answer.")?;
            Ok(answer.to_string())
        }
    }

    impl DirectConnection for WebRtcConnection {
        fn is_open(&self) -> bool {
            self.open.load(Ordering::SeqCst)
                && self.peer_connection.connection_state() == PeerConnectionState::Connected
        }

        fn send(&self, message: &[u8]) -> bool {
            if !self.is_open() {
                return false;
            }
            match self.channel.lock().as_ref() {
                Some(channel) => match channel.send(message, true) {
                    Ok(()) => true,
                    Err(error) => {
                        log::debug!("Praxis Remote direct link refused a message: {error}");
                        false
                    }
                },
                None => false,
            }
        }
    }

    impl Drop for WebRtcConnection {
        fn drop(&mut self) {
            let channel = self.channel.lock().take();
            if let Some(channel) = channel {
                channel.close();
            }
            self.peer_connection.close();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc;
        use std::time::Instant;

        fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !ready() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        /// A phone-side peer, as the app builds it: it creates the channel and
        /// offers, and Praxis answers.
        #[test]
        fn test_direct_link_carries_messages_both_ways() -> Result<()> {
            let factory = PeerConnectionFactory::default();
            let phone = factory.create_peer_connection(rtc_configuration(false))?;
            let phone_channel = phone.create_data_channel("praxis", DataChannelInit::default())?;
            let (phone_received, phone_inbox) = mpsc::channel::<Vec<u8>>();
            let phone_received = Mutex::new(phone_received);
            phone_channel.on_message(Some(Box::new(move |buffer: DataBuffer| {
                phone_received.lock().send(buffer.data.to_vec()).ok();
            })));

            let offer = futures::executor::block_on(phone.create_offer(OfferOptions::default()))?;
            futures::executor::block_on(phone.set_local_description(offer))?;
            wait_until("the phone's addresses", || {
                phone.ice_gathering_state() == IceGatheringState::Complete
            });
            let offer = phone
                .current_local_description()
                .context("the phone has an offer")?
                .to_string();

            let (desktop_received, desktop_inbox) = mpsc::channel::<Vec<u8>>();
            let desktop_received = Mutex::new(desktop_received);
            let desktop = WebRtcConnection::with_configuration(
                &factory,
                rtc_configuration(false),
                Box::new(move |message: Vec<u8>| {
                    desktop_received.lock().send(message).ok();
                }),
            )?;
            let answer = futures::executor::block_on(desktop.answer(&offer))?;
            futures::executor::block_on(
                phone.set_remote_description(SessionDescription::parse(&answer, SdpType::Answer)?),
            )?;

            wait_until("the direct link to open", || {
                desktop.is_open() && phone_channel.state() == DataChannelState::Open
            });
            assert!(phone_channel.send(b"from the phone", true).is_ok());
            assert_eq!(
                desktop_inbox.recv_timeout(Duration::from_secs(10))?,
                b"from the phone"
            );
            assert!(desktop.send(b"from Praxis"));
            assert_eq!(
                phone_inbox.recv_timeout(Duration::from_secs(10))?,
                b"from Praxis"
            );

            phone.close();
            wait_until("Praxis to notice the phone left", || !desktop.is_open());
            assert!(!desktop.send(b"after closing"), "a closed link falls back to the relays");
            Ok(())
        }

        #[test]
        fn test_direct_link_refuses_an_invalid_offer() {
            let factory = PeerConnectionFactory::default();
            let desktop = WebRtcConnection::with_configuration(
                &factory,
                rtc_configuration(false),
                Box::new(|_: Vec<u8>| {}),
            )
            .expect("a peer connection");
            assert!(futures::executor::block_on(desktop.answer("not a session offer")).is_err());
            assert!(!desktop.is_open());
        }
    }
}
