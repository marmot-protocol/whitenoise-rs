use crate::WhitenoiseError;
use nostr_sdk::prelude::*;

pub async fn create_test_client(relays: &[String], keys: Keys) -> Result<Client, WhitenoiseError> {
    let client = Client::default();
    for relay in relays {
        client.add_relay(relay.as_str()).await?;
    }

    client.connect().await;
    client.set_signer(keys).await;

    Ok(client)
}

pub async fn publish_test_metadata(
    client: &Client,
    name: &str,
    about: &str,
) -> Result<(), WhitenoiseError> {
    let metadata = Metadata {
        name: Some(name.to_string()),
        display_name: Some(name.to_string()),
        about: Some(about.to_string()),
        ..Default::default()
    };

    client
        .send_event_builder(EventBuilder::metadata(&metadata))
        .await?;
    Ok(())
}

pub async fn publish_relay_lists(
    client: &Client,
    relay_urls: Vec<String>,
) -> Result<(), WhitenoiseError> {
    let nip65_relay_tags: Vec<Tag> = relay_urls
        .iter()
        .map(|url| {
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::R)),
                [url.clone()],
            )
        })
        .collect();

    let relay_tags: Vec<Tag> = relay_urls
        .iter()
        .map(|url| Tag::custom(TagKind::Relay, [url.clone()]))
        .collect();

    client
        .send_event_builder(EventBuilder::new(Kind::RelayList, "").tags(nip65_relay_tags))
        .await?;
    client
        .send_event_builder(EventBuilder::new(Kind::InboxRelays, "").tags(relay_tags.clone()))
        .await?;
    client
        .send_event_builder(
            EventBuilder::new(Kind::MlsKeyPackageRelays, "").tags(relay_tags.clone()),
        )
        .await?;

    Ok(())
}

pub async fn publish_follow_list(
    client: &Client,
    contacts: &[PublicKey],
) -> Result<(), WhitenoiseError> {
    let tags: Vec<Tag> = contacts
        .iter()
        .map(|pk| Tag::custom(TagKind::p(), [pk.to_hex()]))
        .collect();

    client
        .send_event_builder(EventBuilder::new(Kind::ContactList, "").tags(tags))
        .await?;
    Ok(())
}
