use std::collections::BTreeSet;

use nostr_sdk::PublicKey;

mod upgrade_required_proposals;
mod verify_required_proposals;
mod verify_upgrade_status;

pub use upgrade_required_proposals::UpgradeRequiredProposalsTestCase;
pub use verify_required_proposals::VerifyRequiredProposalsTestCase;
pub use verify_upgrade_status::VerifyRequiredProposalUpgradeStatusTestCase;

pub(super) fn pubkeys_match_unordered(left: &[PublicKey], right: &[PublicKey]) -> bool {
    left.iter().copied().collect::<BTreeSet<_>>() == right.iter().copied().collect::<BTreeSet<_>>()
}

#[cfg(test)]
mod tests {
    use nostr_sdk::PublicKey;

    use super::pubkeys_match_unordered;

    #[test]
    fn pubkeys_match_unordered_ignores_order() {
        let first =
            PublicKey::parse("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let second =
            PublicKey::parse("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();

        assert!(pubkeys_match_unordered(&[first, second], &[second, first],));
    }

    #[test]
    fn pubkeys_match_unordered_rejects_different_sets() {
        let first =
            PublicKey::parse("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let second =
            PublicKey::parse("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();

        assert!(!pubkeys_match_unordered(&[first], &[second]));
    }
}
