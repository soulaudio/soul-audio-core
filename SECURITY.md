# Security Policy

## Reporting a vulnerability

Please report security issues **privately** via GitHub's
[private vulnerability reporting](https://github.com/soulaudio/soul-audio-core/security/advisories/new)
(the repository's **Security** tab → *Report a vulnerability*). Do not open a
public issue for security problems.

We aim to acknowledge a report within a few days and will coordinate a fix and
disclosure with you.

## Supported versions

The project is pre-1.0; security fixes land on `main`. Depend on a specific tag
or commit if you need a stable reference.

## Supply-chain posture

- **Advisories**: `cargo audit` (RustSec) runs on every push, every PR, and
  weekly on a schedule.
- **Licenses, sources, bans**: enforced by `cargo-deny` (`deny.toml`).
- **SBOM**: a CycloneDX SBOM is produced in CI and attached to every release.
- **Dependency updates**: automated via Dependabot (weekly).
- **Actions**: all GitHub Actions are pinned to full commit SHAs.
- **Safety**: the core library is `#![forbid(unsafe_code)]`.
