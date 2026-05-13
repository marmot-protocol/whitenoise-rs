use crate::api::relays::Relay;
use crate::api::wn;
use crate::api::{ApiError, metadata::FlutterMetadata};
use crate::frb_generated::StreamSink;
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use whitenoise::{
    KeyPackageStatus as WhitenoiseKeyPackageStatus, RelayType, User as WhitenoiseUser,
    UserUpdate as WhitenoiseUserUpdate, UserUpdateTrigger as WhitenoiseUserUpdateTrigger,
    Whitenoise,
};

#[frb]
#[derive(Debug, Clone)]
pub enum KeyPackageStatus {
    Valid,
    NotFound,
    Incompatible,
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct User {
    pub pubkey: String,
    pub metadata: FlutterMetadata,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<WhitenoiseUser> for User {
    fn from(user: WhitenoiseUser) -> Self {
        Self {
            pubkey: user.pubkey.to_hex(),
            metadata: user.metadata.into(),
            created_at: user.created_at,
            updated_at: user.updated_at,
        }
    }
}

#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserUpdateTrigger {
    UserCreated,
    MetadataChanged,
    LocalMetadataChanged,
}

impl From<WhitenoiseUserUpdateTrigger> for UserUpdateTrigger {
    fn from(trigger: WhitenoiseUserUpdateTrigger) -> Self {
        match trigger {
            WhitenoiseUserUpdateTrigger::UserCreated => Self::UserCreated,
            WhitenoiseUserUpdateTrigger::MetadataChanged => Self::MetadataChanged,
            WhitenoiseUserUpdateTrigger::LocalMetadataChanged => Self::LocalMetadataChanged,
        }
    }
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct UserUpdate {
    pub trigger: UserUpdateTrigger,
    pub user: User,
}

impl From<WhitenoiseUserUpdate> for UserUpdate {
    fn from(update: WhitenoiseUserUpdate) -> Self {
        Self {
            trigger: update.trigger.into(),
            user: update.user.into(),
        }
    }
}

#[frb]
#[derive(Debug, Clone)]
pub enum UserStreamItem {
    InitialSnapshot { user: User },
    Update { update: UserUpdate },
}

async fn resolve_whitenoise_user(
    whitenoise: &Whitenoise,
    pubkey: &PublicKey,
    blocking_data_sync: bool,
) -> Result<WhitenoiseUser, ApiError> {
    if blocking_data_sync {
        Ok(whitenoise.resolve_user_blocking(pubkey).await?)
    } else {
        Ok(whitenoise.resolve_user(pubkey).await?)
    }
}

#[frb]
pub async fn subscribe_to_user(
    pubkey: String,
    sink: StreamSink<UserStreamItem>,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let subscription = whitenoise.subscribe_to_user(&pubkey).await?;

    if sink
        .add(UserStreamItem::InitialSnapshot {
            user: subscription.initial_user.into(),
        })
        .is_err()
    {
        return Ok(());
    }

    let mut rx = subscription.updates;
    loop {
        match rx.recv().await {
            Ok(update) => {
                if sink
                    .add(UserStreamItem::Update {
                        update: update.into(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    Ok(())
}

#[frb]
pub async fn get_user(pubkey: String, blocking_data_sync: bool) -> Result<User, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let user = resolve_whitenoise_user(whitenoise, &pubkey, blocking_data_sync).await?;
    Ok(user.into())
}

#[frb]
pub async fn user_metadata(
    pubkey: String,
    blocking_data_sync: bool,
) -> Result<FlutterMetadata, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let user = resolve_whitenoise_user(whitenoise, &pubkey, blocking_data_sync).await?;
    Ok(user.metadata.into())
}

#[frb]
pub async fn user_relays(
    pubkey: String,
    relay_type: RelayType,
    blocking_data_sync: bool,
) -> Result<Vec<Relay>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let user = resolve_whitenoise_user(whitenoise, &pubkey, blocking_data_sync).await?;
    let relays = user.relays_by_type(relay_type, &whitenoise).await?;
    Ok(relays.into_iter().map(|r| r.into()).collect())
}

#[frb]
pub async fn user_has_key_package(
    pubkey: String,
    blocking_data_sync: bool,
) -> Result<KeyPackageStatus, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let user = resolve_whitenoise_user(whitenoise, &pubkey, blocking_data_sync).await?;
    match user.key_package_status(&whitenoise.shared).await? {
        WhitenoiseKeyPackageStatus::Valid(_) => Ok(KeyPackageStatus::Valid),
        WhitenoiseKeyPackageStatus::NotFound => Ok(KeyPackageStatus::NotFound),
        WhitenoiseKeyPackageStatus::Incompatible => Ok(KeyPackageStatus::Incompatible),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_update_trigger_conversion_user_created() {
        let trigger: UserUpdateTrigger = WhitenoiseUserUpdateTrigger::UserCreated.into();
        assert_eq!(trigger, UserUpdateTrigger::UserCreated);
    }

    #[test]
    fn test_user_update_trigger_conversion_metadata_changed() {
        let trigger: UserUpdateTrigger = WhitenoiseUserUpdateTrigger::MetadataChanged.into();
        assert_eq!(trigger, UserUpdateTrigger::MetadataChanged);
    }

    #[test]
    fn test_user_update_trigger_conversion_local_metadata_changed() {
        let trigger: UserUpdateTrigger = WhitenoiseUserUpdateTrigger::LocalMetadataChanged.into();
        assert_eq!(trigger, UserUpdateTrigger::LocalMetadataChanged);
    }
}
