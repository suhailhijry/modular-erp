# erp-storage

Where a file actually lives: an engine, a key, and a checksum that is verified
on read.

## What it knows, and what it must never learn

It knows bytes, a key, a checksum, and how to put and get them. It does not know
what a file is *for*, who it belongs to, or who may read it — those are
`modules/files`', and the day this crate learns any of them is the day it stops
being swappable for a different engine.

## An event stores a key, never a URL

**A URL is where a file is today; a key is what it is.** A tenant who moves from
local disk to object storage, or from one bucket to another, has not changed any
of their documents — and an event log full of `https://…/bucket-2019/…` would
say otherwise for ever, in a record nobody can edit.

So what a module writes down is `Stored { engine, key, checksum, size,
media_type }`. Turning that into somewhere a browser can fetch is a *read-time*
concern, and it happens in the handler that already knows who is asking.

## The checksum is verified on read

`fetch` recomputes SHA-256 from what came back and refuses on a mismatch. A
document that comes back different from what was stored is a **failure**, not a
warning: it means the object store lost a write, a disk went bad, or something
wrote over it — and handing a customer a corrupted invoice with a note attached
is worse than handing them nothing. L6.

The handler answers `500` with `storage.corrupt`, because the caller asked for a
document this system said it had and it cannot produce it. That is ours, not
theirs.

## Why the tenant chooses

A module may ship to a customer's own cloud (D15), and a business that keeps its
documents on its own hardware is not a configuration detail — it is the reason
some of them can buy this at all. So the engine is a trait, and which one a
tenant uses is a deployment fact `erp-web` holds as
`AppState::storage: Option<Arc<dyn Storage>>`.

`None` is not a degraded mode: an upload **refuses** with `files.no_storage`
rather than dropping the bytes, the same call the sealing key makes. A tenant
told their contract uploaded when it went nowhere is worse served than one told
it did not.

## `Local`, and what it is honestly for

A development machine with no object store, and a tenant who keeps their own
documents. It writes beside and renames, so a crash halfway through leaves no
file whose checksum will never match.

It is **not** for more than one process unless they share a filesystem: two API
pods with two local roots each hold half a tenant's files and neither knows.
That is a deployment decision rather than a bug in the file, and it is why the
engine is recorded on every stored document — a tenant that outgrows it can be
moved one file at a time, with the log saying exactly which ones have moved.

## Keys are generated, never typed

`check_key` is the traversal check, in one place, before any engine sees the
string: no leading or trailing slash, no `..`, no `.`, and ASCII alphanumerics
plus `- _ . /`. A key becomes a path on local disk and an object name in a
bucket, and `../` means something in the first and nothing in the second —
refusing it here is what stops one engine's rules leaking into the other's
safety.

The name a person gave the file is on the record and **not** in the key: a key
with a filename in it is a key with a space, a slash or an Arabic character in
it, and three engines with three opinions about each.

## What is deliberately absent

**An S3 engine.** The trait is the seam and an implementation is one `impl`, but
it needs a dependency decision — an AWS SDK, or SigV4 signing written here — and
it cannot be verified against anything in this build. Flagged for review rather
than guessed at, the same call the messaging provider adapters got.

**Streaming.** A file is read into memory, which is what caps it at `MAX_BYTES`
(25 MiB). A tenant who needs more needs a different shape rather than a bigger
number.
