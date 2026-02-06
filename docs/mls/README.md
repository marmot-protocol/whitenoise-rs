# MLS Protocol Documentation

This directory contains documentation and references for the MLS (Messaging Layer Security) protocol implementation in Whitenoise.

## Structure

- `rfc9420.txt` - Complete RFC 9420 specification (core MLS protocol)
- `rfc9750.txt` - Complete RFC 9750 specification (MLS architecture)
- `marmot-implementation.md` - Implementation notes for Marmot Protocol
- `mdk-integration.md` - Integration details for the MDK crate

## Quick Reference Links

### Official Specifications
- [RFC 9420 - MLS Protocol](https://www.rfc-editor.org/rfc/rfc9420.html)
- [RFC 9750 - MLS Architecture](https://www.rfc-editor.org/rfc/rfc9750.html)
- [Marmot - Official Specification](https://github.com/marmot-protocol/marmot)
- [MDK](https://github.com/marmot-protocol/mdk)

### Implementation Resources
- [MLS Working Group](https://datatracker.ietf.org/wg/mls/about/)

## For LLMs and AI Tools

This documentation is structured to help AI coding assistants understand the MLS protocol context when working on this project. Key areas to reference:

1. **Protocol Understanding**: Start with `rfc9420.txt` for the core MLS specification
2. **Architecture**: See `rfc9750.txt` for the MLS architecture overview
3. **Implementation Details**: Check `mdk-integration.md` for library-specific patterns
4. **Nostr Integration**: See `marmot-implementation.md` for how MLS maps to Nostr events

When modifying MLS-related code, always consider the security implications and protocol requirements documented here.
