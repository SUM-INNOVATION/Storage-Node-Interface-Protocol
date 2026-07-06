# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| `0.4.x` (current) | Yes |
| `< 0.4` | No |

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Report vulnerabilities by emailing the SUM Innovation security team:

**michael@suminnovation.xyz**

Include:
- A description of the vulnerability and its impact
- Steps to reproduce or a proof-of-concept
- Affected version(s) and platform(s)
- Any suggested mitigations, if known

You will receive an acknowledgement within **48 hours** and a triage decision within **7 days**. We will coordinate a disclosure timeline with you before any public release.

## Scope

In scope:
- `sum-node` binary (archive and client modes)
- `sum-net`, `sum-store`, `sum-crypto`, `sum-types` crates
- The SNIP V2 protocol wire format and chain integration

Out of scope:
- SUM Chain itself (report chain-level issues to the chain team)
- Third-party dependencies (report upstream; cc us if SNIP is the affected consumer)
- Issues requiring physical access to an operator's machine

## Known limitations

**Revocation does not provide forward secrecy.** Revoking a recipient removes their chain access entry but does not rotate the file encryption key (`K_file`). A revoked recipient who cached ciphertext and their wrapped key bundle prior to revocation can still decrypt past content. In-place key rotation is planned for a future release. Operators requiring forward secrecy should revoke and re-ingest the file under a fresh `K_file`. See [`docs/reference/PRIVACY-AUDIT.md`](docs/reference/PRIVACY-AUDIT.md) (threat 14).

## Security documentation

- [`docs/reference/PRIVACY-AUDIT.md`](docs/reference/PRIVACY-AUDIT.md) — full threat-mitigation table with pinning guards
- [`docs/reference/SECURITY-ANALYSIS.md`](docs/reference/SECURITY-ANALYSIS.md) — access control and encryption design analysis
- [`docs/reference/CHAIN-COMPAT.md`](docs/reference/CHAIN-COMPAT.md) — wire-format compatibility guarantees
