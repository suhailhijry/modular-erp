-- Let a tenant that has nothing to do stop being asked.
--
-- # The cost this removes
--
-- `next_visit_at` throttles a quiet tenant to a **fixed** thirty seconds, and it
-- never grows. So a tenant that has been silent for a year is still visited
-- twice a minute, for ever. Across five thousand tenants that is about 167
-- visits a second in perpetuity, almost all of them finding nothing:
--
--     5,000 tenants / 30s  =  167 visits/s, indefinitely
--
-- Each visit opens a connection, runs every enabled module's projection query,
-- checks the outbox, and writes a row back. It is the largest standing cost this
-- platform has, and every unit of it is spent on tenants doing nothing.
--
-- # What replaces it
--
-- Consecutive idle visits back the interval off exponentially, to a cap. A
-- tenant that has just worked is revisited at once; one that has been quiet for
-- a day is asked every few hours; the fleet cost falls with the square of
-- nothing happening.
--
--     100 active + 4,900 dormant  ≈  3.5 visits/s
--
-- **Waking is already built.** `request_visit` pulls `next_visit_at` back to
-- now, and every write calls it. So a dormant tenant that receives a request is
-- current again within a claim cycle, and the backoff is invisible to anyone
-- using the system.
--
-- # Why a counter and not a timestamp
--
-- "How many times running has this found nothing" is what decides the next
-- delay, and it is one integer that the visit already knows the answer to.
-- Deriving it from a `last_worked_at` would need the same write and then a
-- subtraction, and would answer a slightly different question — a tenant that
-- worked once an hour ago and has been idle since is not the same as one that
-- has never worked at all.
ALTER TABLE tenant
    ADD COLUMN IF NOT EXISTS idle_visits INT NOT NULL DEFAULT 0
        CHECK (idle_visits >= 0);

COMMENT ON COLUMN tenant.idle_visits IS
    'Consecutive visits that found nothing. Reset to 0 by a visit that worked; \
     drives the exponential backoff in WorkSchedule::next_idle_delay.';
