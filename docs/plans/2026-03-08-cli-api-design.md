# CLI + REST API Design

**Date:** 2026-03-08
**Status:** Approved (conversation)

## Architecture

```
Claude Code → Bash("whatsrust send ...") → HTTP → running whatsrust daemon
```

## Changes

### New: src/api.rs — REST API server
Raw TCP, no framework. Replaces spawn_health_server.
- GET /api/status, /api/qr, /api/groups, /api/group-info
- POST /api/send, /api/reply, /api/edit, /api/react
- POST /api/image, /api/video, /api/audio, /api/doc, /api/sticker
- POST /api/location, /api/contact, /api/forward, /api/poll
- All return JSON. Media endpoints take file paths.

### Modified: src/bridge.rs
- Add `metrics: Arc<BridgeMetrics>` to WhatsAppBridge struct
- Expose metrics(), queue_depth() getters
- Move health server spawning out of run_bridge → main.rs spawns api::serve()
- Delete spawn_health_server

### Modified: src/main.rs
- CLI mode: parse args, HTTP client to daemon, print JSON, exit
- Daemon mode: spawn api::serve() after bridge creation
- Port: WHATSRUST_PORT env (fallback HEALTH_PORT), default 7270

### New: Claude Code Skill
~/.claude/skills/whatsrust.md — teaches Claude all CLI commands
