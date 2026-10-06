use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use futures::StreamExt as _;
use nostr_sdk::prelude::*;

#[path = "../crates/agent_ui/src/remote/crypto.rs"]
mod crypto;

const KIND: u16 = 21761;

async fn probe(url: &str) -> Result<Vec<u128>> {
    let desktop = Client::default();
    let phone = Client::default();
    let result = async {
        let key = [7; 32];
        let channel = crypto::random_id()?;
        let phone_id = crypto::random_id()?;
        let desktop_keys = Keys::new(SecretKey::from_slice(&crypto::relay_secret(&key, &channel, &phone_id, "desktop")?)?);
        let phone_keys = Keys::new(SecretKey::from_slice(&crypto::relay_secret(&key, &channel, &phone_id, "phone")?)?);
        desktop.add_relay(url).await?;
        phone.add_relay(url).await?;
        let mut desktop_events = desktop.notifications();
        let mut phone_events = phone.notifications();
        println!("{url}: connecting synthetic peers");
        desktop.connect().await;
        phone.connect().await;
        desktop.wait_for_connection(Duration::from_secs(8)).await;
        phone.wait_for_connection(Duration::from_secs(8)).await;
        println!("{url}: subscribing synthetic peers");
        desktop.subscribe(Filter::new().author(phone_keys.public_key()).pubkey(desktop_keys.public_key()).kind(Kind::from(KIND)).limit(0)).await?;
        phone.subscribe(Filter::new().author(desktop_keys.public_key()).pubkey(phone_keys.public_key()).kind(Kind::from(KIND)).limit(0)).await?;
        while let Some(notification) = desktop_events.next().await {
            if matches!(notification, ClientNotification::Message { message, .. } if matches!(message.as_ref(), RelayMessage::EndOfStoredEvents(_))) { break; }
        }
        while let Some(notification) = phone_events.next().await {
            if matches!(notification, ClientNotification::Message { message, .. } if matches!(message.as_ref(), RelayMessage::EndOfStoredEvents(_))) { break; }
        }
        println!("{url}: subscriptions ready; exchanging three packets each way");
        let mut samples = Vec::new();
        for index in 0..3 {
            let plain = format!("Praxis synthetic transport probe {index}");
            let aad = crypto::relay_aad(&channel, &phone_id, "request");
            let event = EventBuilder::new(Kind::from(KIND), crypto::seal(&key, &aad, plain.as_bytes())?)
                .tags([Tag::public_key(desktop_keys.public_key())]).finalize(&phone_keys)?;
            let started = Instant::now();
            phone.send_event(&event).ok_timeout(Duration::from_secs(3)).await?;
            loop {
                let notification = desktop_events.next().await.context("desktop stream ended")?;
                let ClientNotification::Event { event, .. } = notification else { continue; };
                event.verify()?;
                ensure!(crypto::open(&key, &aad, &event.content)? == plain.as_bytes(), "request payload mismatch");
                break;
            }
            let aad = crypto::relay_aad(&channel, &phone_id, "desktop");
            let event = EventBuilder::new(Kind::from(KIND), crypto::seal(&key, &aad, plain.as_bytes())?)
                .tags([Tag::public_key(phone_keys.public_key())]).finalize(&desktop_keys)?;
            desktop.send_event(&event).ok_timeout(Duration::from_secs(3)).await?;
            loop {
                let notification = phone_events.next().await.context("phone stream ended")?;
                let ClientNotification::Event { event, .. } = notification else { continue; };
                event.verify()?;
                ensure!(crypto::open(&key, &aad, &event.content)? == plain.as_bytes(), "response payload mismatch");
                break;
            }
            samples.push(started.elapsed().as_millis());
        }
        Ok(samples)
    }.await;
    desktop.shutdown().await;
    phone.shutdown().await;
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut passed = 0;
    for relay in ["wss://relay.damus.io", "wss://relay.primal.net"] {
        match tokio::time::timeout(Duration::from_secs(40), probe(relay)).await {
            Ok(Ok(samples)) => {
                println!("{relay}: encrypted round-trip milliseconds {samples:?}");
                passed += 1;
            }
            Ok(Err(error)) => println!("{relay}: unavailable or rejected: {error:#}"),
            Err(_) => println!("{relay}: timed out after 40 seconds"),
        }
    }
    ensure!(
        passed > 0,
        "No tested relay delivered the synthetic encrypted round trip"
    );
    println!("{passed}/2 relays delivered the probe. This is not a capacity or uptime guarantee.");
    Ok(())
}
