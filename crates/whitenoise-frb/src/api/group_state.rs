use crate::api::error::ApiError;
use crate::api::utils::group_id_from_string;
use crate::api::wn;
use crate::frb_generated::StreamSink;
use flutter_rust_bridge::frb;
use nostr_sdk::PublicKey;
use whitenoise::whitenoise::group_state_streaming::GroupStateUpdate as WhitenoiseGroupStateUpdate;

/// Real-time group state event for the subscribed `(account, group)` pair.
///
/// Group state events are membership transitions, not persistent state, so the
/// stream has no initial snapshot — it only emits as transitions happen.
#[frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupStateUpdate {
    /// The account voluntarily left the group.
    LeftGroup,
    /// The account was removed from the group by an admin.
    RemovedFromGroup,
}

impl From<WhitenoiseGroupStateUpdate> for GroupStateUpdate {
    fn from(update: WhitenoiseGroupStateUpdate) -> Self {
        match update {
            WhitenoiseGroupStateUpdate::LeftGroup => Self::LeftGroup,
            WhitenoiseGroupStateUpdate::RemovedFromGroup => Self::RemovedFromGroup,
        }
    }
}

/// Subscribe to real-time group state events for a single `(account, group)` pair.
///
/// Streams [`GroupStateUpdate::LeftGroup`] when the account leaves the group
/// (e.g. via SelfRemove) and [`GroupStateUpdate::RemovedFromGroup`] when an
/// admin removes the account. Real-time only — there is no initial snapshot.
///
/// Open chat views can subscribe alongside `subscribe_to_group_messages` to
/// react when membership ends without also driving a chat-list subscription.
///
/// Returns `ApiError::Whitenoise` (carrying `WhitenoiseError::GroupNotFound`)
/// if the `(account, group)` pair has no AccountGroup row.
#[frb]
pub async fn subscribe_to_group_state(
    account_pubkey: String,
    mls_group_id: String,
    sink: StreamSink<GroupStateUpdate>,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&mls_group_id)?;

    let subscription = whitenoise
        .subscribe_to_group_state(&pubkey, &group_id)
        .await?;
    let mut rx = subscription.updates;

    loop {
        match rx.recv().await {
            Ok(update) => {
                let item: GroupStateUpdate = update.into();
                if sink.add(item).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_state_update_conversion_left_group() {
        let update: GroupStateUpdate = WhitenoiseGroupStateUpdate::LeftGroup.into();
        assert_eq!(update, GroupStateUpdate::LeftGroup);
    }

    #[test]
    fn test_group_state_update_conversion_removed_from_group() {
        let update: GroupStateUpdate = WhitenoiseGroupStateUpdate::RemovedFromGroup.into();
        assert_eq!(update, GroupStateUpdate::RemovedFromGroup);
    }
}
