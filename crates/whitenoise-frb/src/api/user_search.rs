use crate::api::error::ApiError;
use crate::api::metadata::FlutterMetadata;
use crate::api::wn;
use crate::frb_generated::StreamSink;
use flutter_rust_bridge::frb;
use nostr_sdk::PublicKey;
use whitenoise::whitenoise::user_search::UserSearchParams;
use whitenoise::{
    MatchQuality as WnMatchQuality, MatchedField as WnMatchedField,
    SearchUpdateTrigger as WnSearchUpdateTrigger, UserSearchResult as WnUserSearchResult,
    UserSearchUpdate as WnUserSearchUpdate,
};

#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchQuality {
    Exact,
    Prefix,
    Contains,
}

impl From<WnMatchQuality> for MatchQuality {
    fn from(quality: WnMatchQuality) -> Self {
        match quality {
            WnMatchQuality::Exact => Self::Exact,
            WnMatchQuality::Prefix => Self::Prefix,
            WnMatchQuality::Contains => Self::Contains,
        }
    }
}

#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchedField {
    Name,
    Nip05,
    DisplayName,
    About,
}

impl From<WnMatchedField> for MatchedField {
    fn from(field: WnMatchedField) -> Self {
        match field {
            WnMatchedField::Name => Self::Name,
            WnMatchedField::Nip05 => Self::Nip05,
            WnMatchedField::DisplayName => Self::DisplayName,
            WnMatchedField::About => Self::About,
        }
    }
}

#[frb]
#[derive(Debug, Clone)]
pub enum SearchUpdateTrigger {
    RadiusStarted {
        radius: u8,
    },
    ResultsFound,
    RadiusCompleted {
        radius: u8,
        total_pubkeys_searched: u64,
    },
    RadiusTimeout {
        radius: u8,
    },
    SearchCompleted {
        final_radius: u8,
        total_results: u64,
    },
    Error {
        message: String,
    },
}

impl From<WnSearchUpdateTrigger> for SearchUpdateTrigger {
    fn from(trigger: WnSearchUpdateTrigger) -> Self {
        match trigger {
            WnSearchUpdateTrigger::RadiusStarted { radius } => Self::RadiusStarted { radius },
            WnSearchUpdateTrigger::ResultsFound => Self::ResultsFound,
            WnSearchUpdateTrigger::RadiusCompleted {
                radius,
                total_pubkeys_searched,
            } => Self::RadiusCompleted {
                radius,
                total_pubkeys_searched: total_pubkeys_searched as u64,
            },
            WnSearchUpdateTrigger::RadiusTimeout { radius } => Self::RadiusTimeout { radius },
            WnSearchUpdateTrigger::SearchCompleted {
                final_radius,
                total_results,
            } => Self::SearchCompleted {
                final_radius,
                total_results: total_results as u64,
            },
            WnSearchUpdateTrigger::Error { message } => Self::Error { message },
        }
    }
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct UserSearchResult {
    pub pubkey: String,
    pub metadata: FlutterMetadata,
    pub radius: u8,
    pub match_quality: MatchQuality,
    pub best_field: MatchedField,
    pub matched_fields: Vec<MatchedField>,
}

impl From<WnUserSearchResult> for UserSearchResult {
    fn from(result: WnUserSearchResult) -> Self {
        Self {
            pubkey: result.pubkey.to_hex(),
            metadata: result.metadata.into(),
            radius: result.radius,
            match_quality: result.match_quality.into(),
            best_field: result.best_field.into(),
            matched_fields: result
                .matched_fields
                .into_iter()
                .map(|f| f.into())
                .collect(),
        }
    }
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct UserSearchUpdate {
    pub trigger: SearchUpdateTrigger,
    pub new_results: Vec<UserSearchResult>,
    pub total_result_count: u64,
}

impl From<WnUserSearchUpdate> for UserSearchUpdate {
    fn from(update: WnUserSearchUpdate) -> Self {
        Self {
            trigger: update.trigger.into(),
            new_results: update.new_results.into_iter().map(|r| r.into()).collect(),
            total_result_count: update.total_result_count as u64,
        }
    }
}

#[frb]
pub async fn search_users(
    account_pubkey: String,
    query: String,
    radius_start: u8,
    radius_end: u8,
    sink: StreamSink<UserSearchUpdate>,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;

    let params = UserSearchParams {
        query,
        searcher_pubkey: pubkey,
        radius_start,
        radius_end,
    };

    let subscription = whitenoise.search_users(params).await?;
    let mut rx = subscription.updates;

    loop {
        match rx.recv().await {
            Ok(update) => {
                if sink.add(update.into()).is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Slow consumer missed some updates - some search results
                // may be lost but total_result_count remains accurate
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
    fn match_quality_conversion_exact() {
        let quality: MatchQuality = WnMatchQuality::Exact.into();
        assert_eq!(quality, MatchQuality::Exact);
    }

    #[test]
    fn match_quality_conversion_prefix() {
        let quality: MatchQuality = WnMatchQuality::Prefix.into();
        assert_eq!(quality, MatchQuality::Prefix);
    }

    #[test]
    fn match_quality_conversion_contains() {
        let quality: MatchQuality = WnMatchQuality::Contains.into();
        assert_eq!(quality, MatchQuality::Contains);
    }

    #[test]
    fn matched_field_conversion_name() {
        let field: MatchedField = WnMatchedField::Name.into();
        assert_eq!(field, MatchedField::Name);
    }

    #[test]
    fn matched_field_conversion_nip05() {
        let field: MatchedField = WnMatchedField::Nip05.into();
        assert_eq!(field, MatchedField::Nip05);
    }

    #[test]
    fn matched_field_conversion_display_name() {
        let field: MatchedField = WnMatchedField::DisplayName.into();
        assert_eq!(field, MatchedField::DisplayName);
    }

    #[test]
    fn matched_field_conversion_about() {
        let field: MatchedField = WnMatchedField::About.into();
        assert_eq!(field, MatchedField::About);
    }

    #[test]
    fn search_update_trigger_conversion_radius_started() {
        let trigger: SearchUpdateTrigger =
            WnSearchUpdateTrigger::RadiusStarted { radius: 2 }.into();
        assert!(matches!(
            trigger,
            SearchUpdateTrigger::RadiusStarted { radius: 2 }
        ));
    }

    #[test]
    fn search_update_trigger_conversion_results_found() {
        let trigger: SearchUpdateTrigger = WnSearchUpdateTrigger::ResultsFound.into();
        assert!(matches!(trigger, SearchUpdateTrigger::ResultsFound));
    }

    #[test]
    fn search_update_trigger_conversion_radius_completed() {
        let trigger: SearchUpdateTrigger = WnSearchUpdateTrigger::RadiusCompleted {
            radius: 1,
            total_pubkeys_searched: 500,
        }
        .into();
        assert!(matches!(
            trigger,
            SearchUpdateTrigger::RadiusCompleted {
                radius: 1,
                total_pubkeys_searched: 500,
            }
        ));
    }

    #[test]
    fn search_update_trigger_conversion_radius_timeout() {
        let trigger: SearchUpdateTrigger =
            WnSearchUpdateTrigger::RadiusTimeout { radius: 4 }.into();
        assert!(matches!(
            trigger,
            SearchUpdateTrigger::RadiusTimeout { radius: 4 }
        ));
    }

    #[test]
    fn search_update_trigger_conversion_search_completed() {
        let trigger: SearchUpdateTrigger = WnSearchUpdateTrigger::SearchCompleted {
            final_radius: 2,
            total_results: 42,
        }
        .into();
        assert!(matches!(
            trigger,
            SearchUpdateTrigger::SearchCompleted {
                final_radius: 2,
                total_results: 42,
            }
        ));
    }

    #[test]
    fn search_update_trigger_conversion_error() {
        let trigger: SearchUpdateTrigger = WnSearchUpdateTrigger::Error {
            message: "something broke".to_string(),
        }
        .into();
        if let SearchUpdateTrigger::Error { message } = trigger {
            assert_eq!(message, "something broke");
        } else {
            panic!("Expected Error variant");
        }
    }
}
