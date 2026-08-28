# Backup and restore

The procedure below runs as a test on every build. A backup nobody has restored
isn't a backup, and once a customer is hosting the system a failed restore
happens somewhere we can't reach.

## What to save

There are two things and they aren't independent of each other.

The control database holds the tenant register, the people and their
memberships. Without it, a tenant database has no route to it.

Each tenant database holds the event log, and nothing else can reconstruct that
log, since nothing is able to change it after a write.

You could skip the read models, because the system can rebuild them. Don't. A
rebuild runs at roughly four thousand events a second, so a tenant with a few
million events costs you a quarter of an hour to avoid backing up tables that
`pg_dump` would have compressed anyway. Keep the option for the day a backup
turns out to be corrupt.

## Saving

```bash
pg_dump --format=custom --no-owner --no-privileges --file control.dump "$CONTROL_DATABASE_URL"
```

Then one per tenant, taking the database name from the tenant register.

## Restoring

The database has to exist and be empty first:

```bash
psql "$CLUSTER_URL/postgres" -c "CREATE DATABASE \"$DATABASE\""
```

```bash
pg_restore --no-owner --no-privileges --dbname "$CLUSTER_URL/$DATABASE" "$SLUG.dump"
```

## The part that goes wrong

Restore both planes to the same point in time, then check the tenant can
actually be entered. A restore that stops at "the database is back" hasn't
finished.

Because the two are saved separately they can be restored to different moments,
and neither direction reports an error when that happens.

If the control database is older, the tenant database sits there complete and
correct with no route to it. Entering it gets refused with the same message a
stranger receives for a tenant that never existed, since telling those two apart
would let somebody discover which companies use the system.

If the control database is newer, the tenant can be entered and has silently
lost every event after its own backup. Read models rebuilt from it will agree
perfectly with each other and be wrong.
