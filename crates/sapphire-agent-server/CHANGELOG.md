# Changelog

All notable changes to `sapphire-agent-server` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Changes up to `0.8.0`, when this crate was a binary of its own, are recorded in
[`headless/CHANGELOG.md`](../../headless/CHANGELOG.md).

## [Unreleased]

## [0.12.0](https://github.com/sapphire-turtle/sapphire-agent/compare/v0.11.2...v0.12.0) - 2026-11-04

### Changed

- The agent's app server as a library, which both sapphire-agent editions run. Published as a binary crate up to 0.8.0, it resumes at 0.12.0 with the editions' version; see [`headless/CHANGELOG.md`](../../headless/CHANGELOG.md) for the user-visible changes (API keys under room profiles, avatar tools and files, image attachments)
- Built on sapphire-framework 0.26 (server API 6) and sapphire-agent-core 0.11
