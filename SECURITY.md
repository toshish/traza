# Security

## Reporting

Report suspected vulnerabilities privately through GitHub's private
vulnerability reporting:
https://github.com/toshish/traza/security/advisories/new. You will get an
acknowledgement within 72 hours. Please do not open a public issue for a
suspected vulnerability.

## Supported versions

Traza is pre-1.0. Only the latest 0.x release is supported, and fixes ship
as the next release rather than as backports.

## Scope, in one paragraph

The server trusts its bearer tokens and nothing else. Stored span text is
attacker-controlled by assumption: on the MCP surface it is confined to
blocks marked untrusted and never reaches a tool description or an error
message, and the server holds no fetcher, shell, or outbound network path.
In scope: anything that breaks that confinement, the auth gate, scope
enforcement (an `ro` token reaching a write), the durability contract as
documented per mode, or memory safety — the crate is
`#![forbid(unsafe_code)]`, so any memory-safety break is interesting. Not a
finding: the documented-lossy `buffered` mode losing unflushed writes, or
resource exhaustion on an unauthenticated loopback bind you chose to expose.
