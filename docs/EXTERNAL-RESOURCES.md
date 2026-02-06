# External Resources and Documentation Links

This file provides a comprehensive index of external resources, specifications, and documentation that LLMs and developers should reference when working on this project.

## MLS (Messaging Layer Security) Protocol

### Official IETF Specifications
- **RFC 9420 - The Messaging Layer Security (MLS) Protocol**: https://www.rfc-editor.org/rfc/rfc9420.html
  - Primary protocol specification
  - Download as text: `curl -s https://www.rfc-editor.org/rfc/rfc9420.txt > docs/mls/rfc9420.txt`

- **RFC 9750 - MLS Architecture**: https://www.rfc-editor.org/rfc/rfc9750.html
  - Architectural overview and security analysis
  - Download as text: `curl -s https://www.rfc-editor.org/rfc/rfc9750.txt > docs/mls/rfc9750.txt`

### IETF Working Group Resources
- **MLS Working Group**: https://datatracker.ietf.org/wg/mls/about/
- **Draft Specifications**: https://datatracker.ietf.org/wg/mls/documents/
- **Meeting Materials**: https://datatracker.ietf.org/wg/mls/meetings/

### Implementation Resources
- **MLS Implementations List**: https://github.com/mlswg/mls-implementations
- **Test Vectors**: https://github.com/mlswg/mls-implementations/tree/master/test-vectors
- **Interoperability Testing**: https://github.com/mlswg/mls-implementations/tree/master/interop

## Nostr Protocol

### Core Specifications
- **Nostr Protocol**: https://github.com/nostr-protocol/nostr
- **NIPs (Nostr Implementation Possibilities)**: https://github.com/nostr-protocol/nips

### MLS on Nostr
- **NIP-EE - MLS over Nostr**: https://github.com/nostr-protocol/nips/blob/master/EE.md (legacy specification)
- Marmot - Official Specification: https://github.com/marmot-protocol/marmot
- MDK (Marmot Development Kit)- https://github.com/marmot-protocol/mdk

### Rust Nostr Implementation
- **rust-nostr Repository**: https://github.com/rust-nostr/nostr

## Cryptographic Foundations

### Core Cryptography
- **TreeKEM Specification**: https://datatracker.ietf.org/doc/html/draft-ietf-mls-protocol-20#section-7
- **HPKE (Hybrid Public Key Encryption)**: https://www.rfc-editor.org/rfc/rfc9180.html
- **TLS 1.3 Specification**: https://www.rfc-editor.org/rfc/rfc8446.html

### Security Analysis
- **MLS Security Analysis**: https://eprint.iacr.org/2020/1019.pdf
- **TreeKEM Security Proofs**: https://eprint.iacr.org/2020/1013.pdf

## Development Tools and Libraries

### Rust Cryptography
- **RustCrypto**: https://github.com/RustCrypto
- **ring**: https://github.com/briansmith/ring
- **dalek-cryptography**: https://github.com/dalek-cryptography

### Testing and Validation
- **MLS Test Vectors**: https://github.com/mlswg/mls-implementations/tree/master/test-vectors
- **Cryptographic Test Vectors**: https://github.com/google/wycheproof

## Downloading and Caching Documentation

To cache external documentation locally, use these commands:

```bash
# Create directories first
mkdir -p docs/mls docs/external

# Download MLS RFCs
curl -s https://www.rfc-editor.org/rfc/rfc9420.txt > docs/mls/rfc9420.txt
curl -s https://www.rfc-editor.org/rfc/rfc9750.txt > docs/mls/rfc9750.txt

# Download HPKE specification
curl -s https://www.rfc-editor.org/rfc/rfc9180.txt > docs/mls/rfc9180-hpke.txt

# Clone MLS test vectors (if needed)
git clone https://github.com/mlswg/mls-implementations.git docs/external/mls-implementations
```

## Integration Notes

When working with external resources:

1. **Version Pinning**: Always reference specific versions/commits of external specifications
2. **Local Caching**: Download key specifications to `docs/` for offline access
3. **Update Tracking**: Monitor specification updates that may affect our implementation
4. **Compliance Testing**: Use official test vectors to validate implementation compliance

## For LLMs and AI Tools

This file serves as a comprehensive reference index. When working on MLS related code:

1. **Start Here**: Reference this file for authoritative sources
2. **Local First**: Check `docs/` for cached versions before accessing external links
3. **Specification Compliance**: Always validate against official RFCs and test vectors
4. **Security Focus**: Prioritize security analysis documents when making design decisions
