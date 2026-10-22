# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

### Changed

- Migrated the terminal application from Python/Textual to Rust with Ratatui
- Replaced Python packaging and setup instructions with Cargo
- Moved administrator-only commands into a dedicated Windows UAC sidecar

## [0.1.0] - 2026-07-27

### Added

- Terminal interface for viewing connected and persisted USB devices
- Device bind, attach, detach, and unbind actions
- Bulk detach and unbind actions
- Automatic attachment rules for selected bus IDs
- Automatic device-state refresh
- Keyboard shortcuts and mouse-friendly controls
- Windows UAC elevation for administrator-only commands
- Pytest coverage for device state, command execution, and configuration storage

[0.1.0]: https://github.com/gabrieldurante/usbipd-tui/releases/tag/v0.1.0
