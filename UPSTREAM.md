# Upstream relationship

This repository is derived from [Ironbar](https://github.com/JakeStanger/ironbar).
The fork began from Ironbar commit
`5b96bcffac54dd82347badcc07f79d58efa715c7`. Ironbar-derived source preserves
Ironbar's MIT grant and copyright notice in `LICENSES/IRONBAR-MIT.txt` and
`NOTICE`; cbar contributions are provided under FSL-1.1-ALv2 and convert to
Apache-2.0 two years after each version is made available.

## Compatibility contract

The fork keeps Ironbar's configuration formats, module configuration, styling
surface, extension points, and supported compositor scope. Fork-specific
features are additive. A release must pass the compatibility and regression
gates documented in `docs/Cbar architecture.md` before it is published.

Repository, package, application, and release identities are intentionally
separate from upstream ownership. The public product is `corbet-labs/cbar`, its
binary and service are both named `cbar`, and its application ID is
`ch.corbet.cbar`. Every Cargo package remains `publish = false`; releases are
distributed as repository artifacts and Nix packages until crate publication
is evaluated separately.

## Synchronising upstream

The `upstream` remote is fetch-only. After fetching `upstream/master`, integrate
it on a dedicated branch, resolve conflicts without discarding either
upstream-supported behaviour or fork release gates, and run the complete
release matrix before merging. Published fork history is not rebased or
force-pushed merely to make it resemble upstream history.

Record the integrated upstream commit in each release. Never publish or push to
the official Ironbar repository from this working tree.
