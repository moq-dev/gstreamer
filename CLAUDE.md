# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a GStreamer plugin for Media over QUIC (MoQ), written in Rust. It provides `moqsink` and `moqsrc` elements that enable publishing and subscribing to media streams using the MoQ protocol over QUIC transport.

## Development Setup

### Prerequisites
- Rust toolchain (via `rustup`)
- Just command runner
- A running moq-relay server from [moq](https://github.com/kixelated/moq)

### Initial Setup
```bash
# Install dependencies and tools
just setup

# To see all available commands
just
```

## Common Commands

### Building
```bash
# Build the plugin
just build
# or
cargo build
```

### Testing and Quality Checks
```bash
# Run all CI checks (clippy, fmt, cargo check)
just check

# Run tests
just test

# Auto-fix issues
just fix
```

### Development Workflow
```bash
# Start a relay server (in moq repo)
just relay

# Publish video stream with broadcast name
just pub bbb

# Subscribe to video stream with broadcast name
just sub bbb
```

## Architecture

### Plugin Structure
- **lib.rs**: Main plugin entry point, registers both sink and source elements as "moq" plugin
- **sink/**: MoQ sink element (`moqsink`) for publishing streams
  - `mod.rs`: GStreamer element wrapper for MoqSink
  - `imp.rs`: Core implementation with async Tokio runtime
- **source/**: MoQ source element (`moqsrc`) for consuming streams
  - `mod.rs`: GStreamer element wrapper for MoqSrc
  - `imp.rs`: Core implementation with async Tokio runtime

### Key Dependencies
- **hang**: Higher-level protocol utilities and catalog/container handling
- **moq-mux**: MoQ muxing/demuxing for media streams
- **moq-lite**: Lightweight MoQ protocol types
- **moq-native**: Core MoQ protocol implementation with QUIC/TLS
- **gstreamer**: GStreamer bindings for Rust
- **tokio**: Async runtime

### Plugin Elements
- `moqsink`: Element with request pads (`video_%u`, `audio_%u`) that accepts media data and publishes via MoQ
- `moqsrc`: Bin element that receives MoQ streams and outputs GStreamer buffers

Both elements use a shared Tokio runtime and support TLS configuration options (url, broadcast, tls-disable-verify).

## Environment Variables
- `RUST_LOG`: Controls logging level (default: info, overridable via environment)
- `URL`: Relay server URL (default: http://localhost:4443)
- `GST_PLUGIN_PATH`: Must include the built plugin directory (handled automatically by justfile)