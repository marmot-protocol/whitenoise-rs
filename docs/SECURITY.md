# Security Considerations

## Dependency Audit

This project uses `cargo audit` to scan for known security vulnerabilities in dependencies. The audit is configured to ignore certain advisories that have been evaluated and deemed acceptable risks:

### Ignored Advisories

#### RUSTSEC-2023-0071: RSA Marvin Attack
- **Component**: `rsa` crate (transitive dependency via `sqlx-mysql`)
- **Severity**: Medium (5.9 CVSS)
- **Issue**: Potential key recovery through timing side-channels in RSA operations
- **Justification**: This is a transitive dependency through `sqlx-mysql` which is included by sqlx's macro system at compile time. White Noise only uses SQLite for data storage, not MySQL. The vulnerable RSA code is used for MySQL authentication and is never executed in our application's code paths.
- **Risk Assessment**: Low - the vulnerable code path is never active in our application
- **Mitigation**: Monitor for updates to the `rsa` crate and sqlx dependencies

#### RUSTSEC-2024-0384: instant crate unmaintained
- **Component**: `instant` crate (transitive dependency via rust-nostr)
- **Severity**: Warning (unmaintained)
- **Issue**: The `instant` crate is no longer actively maintained
- **Justification**: This is a transitive dependency from the rust-nostr ecosystem. The crate provides basic cross-platform time functionality with minimal attack surface.
- **Risk Assessment**: Low - basic functionality that is unlikely to introduce security issues
- **Mitigation**: Will be resolved when rust-nostr updates to an alternative time library

#### RUSTSEC-2026-0002: lru unsound
- **Component**: `lru` crate (transitive dependency via nostr-sdk)
- **Severity**: Warning (unsound)
- **Issue**: The `lru` crate has a soundness issue
- **Justification**: This is a transitive dependency from the nostr-sdk ecosystem. We do not use the lru crate directly.
- **Risk Assessment**: Low - awaiting upstream fix in nostr-sdk
- **Mitigation**: Will be resolved when nostr-sdk updates to a fixed version of lru

## Running Security Audits

To run the security audit:

```sh
just audit
```

This command runs `cargo audit` with the appropriate ignore flags for the advisories listed above.

## Reporting Security Issues

If you discover a security vulnerability in White Noise, please report it privately by emailing [j@whitenoise.chat](mailto:j@whitenoise.chat). Please do not create public GitHub issues for security vulnerabilities.

## Security Best Practices

White Noise implements several security best practices:

1. **MLS Protocol**: Uses the Messaging Layer Security protocol for end-to-end encrypted group messaging
2. **Forward Secrecy**: Messages cannot be decrypted even if future keys are compromised
3. **Post-Compromise Security**: The system can recover security after a key compromise
4. **Secure Key Storage**: Uses platform-native keychains for sensitive key material
5. **Regular Audits**: Dependencies are regularly scanned for known vulnerabilities

