-- Which web origins may call a tenant's public API from a browser.
--
-- ===========================================================================
-- Why this is control-plane data and not a tenant configuration value
-- ===========================================================================
--
-- Every other per-tenant setting in this system lives in the tenant's own
-- database, read through `erp_eventlog::configuration`. This one cannot,
-- because of *when* it is needed: CORS is decided before any tenant-domain
-- code runs, on requests that carry no session and may not be allowed to reach
-- the tenant's data at all. A preflight `OPTIONS` is answered without opening
-- the tenant database, which is the point of a preflight.
--
-- It is also a **security boundary the platform enforces**, not a preference
-- the business expresses. The same argument that puts entitlements here: what a
-- tenant may do is the platform's answer, and what a tenant wants is theirs.
--
-- ===========================================================================
-- Why an origin is not a domain, and why both columns exist
-- ===========================================================================
--
-- An origin is scheme + host + port — `https://salon.com` — and that triple is
-- what a browser sends and what the response has to echo back exactly. A
-- *domain* is what somebody proves they own. One domain can license several
-- origins (`https://salon.com`, `https://www.salon.com`), and proving ownership
-- once should not have to be done again per subdomain.
--
-- So `domain` is what was verified and `origin` is what is allowed, and the
-- verification state lives on the domain.
CREATE TABLE tenant_domain (
    tenant        UUID NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- The registrable domain, lower case, no scheme and no port: `salon.com`.
    domain        TEXT NOT NULL
                  CHECK (domain = lower(domain)
                     AND domain !~ '[:/]'
                     AND length(domain) BETWEEN 3 AND 253),

    -- What the tenant has to publish to prove they own it. Generated here and
    -- never reused: a token that appeared in a previous tenant's DNS is a token
    -- that could still be there.
    verification_token TEXT NOT NULL,

    -- Null until proved. **Nothing is served cross-origin for an unverified
    -- domain**, which is the whole reason this column is not a boolean with a
    -- default: the absence of a timestamp is the absence of proof, and a
    -- default of `false` invites a migration that flips it.
    verified_at   TIMESTAMPTZ,

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant, domain)
);

-- One domain belongs to one tenant. Two tenants both claiming `salon.com` is
-- either a mistake or an attack, and the second one to try should be told no
-- rather than quietly sharing.
CREATE UNIQUE INDEX tenant_domain_is_exclusive ON tenant_domain (domain);

-- The origins a verified domain licenses.
--
-- Separate from the domain so that adding `https://www.salon.com` alongside
-- `https://salon.com` is a row and not a second verification.
CREATE TABLE tenant_origin (
    tenant        UUID NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- Exactly what a browser sends in `Origin`, lower case: scheme, host and a
    -- port only when it is not the scheme's default. Echoed back verbatim, so
    -- it is stored verbatim.
    origin        TEXT NOT NULL
                  CHECK (origin = lower(origin)
                     AND origin ~ '^https?://[a-z0-9.-]+(:[0-9]{1,5})?$'),

    -- Which verified domain licenses it. `ON DELETE CASCADE`: withdrawing a
    -- domain withdraws every origin under it, in one act, which is what an
    -- operator revoking access means by it.
    domain        TEXT NOT NULL,

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant, origin),
    FOREIGN KEY (tenant, domain) REFERENCES tenant_domain (tenant, domain)
        ON DELETE CASCADE
);

-- The entry-path lookup: every cross-origin request asks this question once.
CREATE INDEX tenant_origin_by_origin_idx ON tenant_origin (origin);
