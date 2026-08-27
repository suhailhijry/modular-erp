# Modular multi-tenant ERP

A BYOC multi-tenant ERP backend. Powered by Rust and Postgres.

## Current Modules

| Module | What it does |
|---|---|
| `ledger` | Double-entry accounting. |
| `sales` | Invoices, credit notes and payments. |
| `purchases` | Bills and input tax. |
| `tax_sa` | Saudi VAT returns and ZATCA e-invoicing integration (submodule of sales and purchases). |

Other features:

- An immutable event log for each tenant. Nothing can change the log after a
  write, enforced at the database level.
- Disposable projections, replayed at any time.
- Modular and enforceable localization support for Arabic & English, with the ability to add more languages in the future.
- Fully self-service-able cloud and SaaS platform.
- Fully documented REST API, with self-generating OpenAPI spec.

## Why you can choose this system

**Your data stays in your cloud if you want to** Bring Your Own Cloud is the deployment model, meaning if you trust us, we can deploy it for you, otherwise, you can use your own infrastructure if you want to.

**Secure by default** State of art security enforcment, and permission management.

**Source available** The license is the Business Source License 1.1. A customer can read the code before they buy it. A security team can audit it.

## How to run

You need Rust, Postgres 18, Docker and `just`.

1. Copy the database settings into `.env`.

2. Start Redis. The port must be 6379 (in the meantime, will be configurable in the future), because the tests use that port.

   ```bash
   just redis
   ```

3. Make the offline query data. Do this again after you change a migration.

   ```bash
   just prepare
   ```

4. Run the format check, the lints and the tests.

   ```bash
   just check
   ```

To start the whole system in containers, use this command:

```bash
docker compose up
```

To make a tenant with data that you can look at, use this command:

```bash
just demo my-password
```

## Technologies

| | |
|---|---|
| Rust | High-performance systems language, chosen for safety and raw performance. |
| Postgres 18 | Chosen for its long-standing resilience and durability. |
| Redis | Chosen for caching |
| `tokio` | The asynchronous runtime. |
| `axum` and `tower` | The HTTP server and its layers. |
| `sqlx` | The database driver. It checks each query when it compiles the code. |
| `utoipa` | Automatic OpenAPI spec from the router |
| `argon2` | Hashing passwords. |
| OpenSSL | Certificates and signatures for ZATCA. |
| `lettre` | For e-mails |
| Docker | Containers for the whole system, with a standby database. |

## Performance

I measured each number below on one developer machine, with a release
build and Postgres 18. Your hardware will probably result in different numbers.

| Measurement | Result | Conditions |
|---|---|---|
| Operations each second | 22,169 | 40 tenants, 256 workers |
| Memory for one API process | 14 MB | After it starts to listen |
| Time to start an API process | 82 ms | Until it accepts the first request |
| Open database connections | 95 | 40 active tenants, 4 connections for each |
| Read model rebuild | 4,096 events each second | 2 projections, 4 lines for each event |
| Background visits | 3.5 each second | 100 active tenants and 4,900 quiet ones |
| Migration of 40 tenants | 36 ms | 32 tenants at the same time |

## License

Business Source License 1.1. See [LICENSE](LICENSE). A source-available license.

The code becomes available under Apache 2.0 four years after each release.
