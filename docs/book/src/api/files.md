# files

Documents attached to things: what was stored, where, and proof it came back
unchanged.

**Depends on:** the core and `erp-storage`.
**Depended on by:** nothing.

## No module dependencies at all

An attachment belongs to an invoice, a bill, a booking, a customer, an employee
record, a journal entry or the business itself. Modelling that as seven event
types — or seven dependencies, so the id could be validated — would make
attaching to the eighth thing a change here.

It is a `(kind, id)` pair, opaque, with no foreign key and no join. The eighth
thing is one more value of an existing enum. The same arrangement
`occupancy_resource` has with `booking`: the module that owns the meaning owns
the id.

## The bytes first, the event second

An orphaned object is wasted space somebody can sweep. A record pointing at
bytes that were never written is a document that cannot be opened, with nothing
to say why. So the handler stores, then records.

Uploading twice is one document (L8): `create` refuses a taken id unless it is a
retry of the request that made it, and the retry writes the same bytes to the
same key — a rewrite of identical content.

## Taking one off does not erase it

`DELETE /v1/files/{file}` writes `files.file.removed` and **leaves the bytes
alone**. A document that was on an invoice is part of what happened, and erasing
it on a click would erase evidence. The record stays and says why it came off —
the same call `crm::archive_customer` makes about never deleting a customer.

Removing the bytes as well is a separate act with its own authority, and nobody
has asked for one.

## The routes, and why the bytes are their own sub-resource

| | |
|---|---|
| `GET /v1/files?owner_kind=&owner_id=` | What is attached to one thing |
| `GET /v1/files/{file}` | The record. Not the bytes |
| `DELETE /v1/files/{file}` | Take it off |
| `POST /v1/files/{file}/content` | Upload |
| `GET /v1/files/{file}/content` | Download |

The split is about the **body limit**. Every other route in this API takes a
small JSON body and is capped at a megabyte; an upload takes a scanned contract.
Keeping them on separate paths is what lets the cap be raised for exactly the
two that need it — axum's `DefaultBodyLimit` is innermost-wins, so the module's
own layer beats the router's default where it is applied and nowhere else.
`the_raised_body_limit_applies_to_uploads_and_nowhere_else` is what holds that
down.

## A download is always an attachment

An uploaded file is somebody else's bytes with somebody else's declared type.
Serving it inline means a browser may render it **in the tenant's own origin**,
and an HTML file uploaded as a "document" then runs as the tenant.

So: `Content-Disposition: attachment` with an RFC 5987 filename, so an Arabic
name survives the header, and `X-Content-Type-Options: nosniff` because a
browser that decides for itself what a file is, is a browser that can be talked
into running it.

The media type is recorded **as declared and never sniffed**, for the same
reason: guessing from the first few bytes is how an HTML file becomes something
a browser renders.

## What is deliberately absent

**Per-document authorization.** The owner is recorded and listing is by owner,
but "may this person see this invoice's attachments" is the same question as
"may this person see this invoice", and this system answers neither per record
yet — a role is tenant-wide. That is Phase 5's rules engine, and inventing a
second, weaker answer here would be a thing to unpick when the real one arrives.

**Thumbnails, previews, virus scanning, text extraction.** Each is a separate
service and none of them is what "can a business keep the signed contract with
the invoice" needs.
