# Golden event files

Real stored payloads, one per `(event, schema version)` that has ever existed.
`tests/upcast.rs` decodes every one on every build.

**These files are append-only. Never edit one.** Each is a byte-level record of
what is sitting in some tenant's log right now — editing it to make a test pass
does not change that data, it just stops the test from telling you the data has
become unreadable.

When an event's shape changes:

1. Bump its version and register the `n → n+1` upcaster.
2. Add `<event>.v<new>.json` here with the new shape.
3. Leave every older file exactly as it is.

A failure here means a build has lost the ability to read events that already
exist — which is the failure the whole versioning apparatus exists to prevent.
