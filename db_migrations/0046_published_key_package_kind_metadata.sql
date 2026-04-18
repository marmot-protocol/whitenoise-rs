-- Track dual-published KeyPackage event metadata.

ALTER TABLE published_key_packages
ADD COLUMN kind INTEGER NOT NULL DEFAULT 443;

ALTER TABLE published_key_packages
ADD COLUMN d_tag TEXT NULL;

CREATE INDEX idx_published_kp_account_hash_ref
ON published_key_packages(account_pubkey, key_package_hash_ref);

CREATE INDEX idx_published_kp_account_kind_d_tag
ON published_key_packages(account_pubkey, kind, d_tag);
