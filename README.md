# A Modular multi-tenant ERP Project

A project I'm working on to test Rust, and try different architectures and designs. Currently, it's leaning into being a bring-your-own-cloud system, but that may change as I explore more.

## Current architecture

- Each tenant gets their own database in a PostgreSQL cluster (for managed-mode), and optionally their own PostgreSQL instance/cluster when going for self-hosted mode.
- Custom event-sourcing implementation that uses PostgreSQL as its backing store, with dead-lettering and other features.
- I tried to isolate the crates as much as possible, with muli-language support, high resilience, durability, and rigorous testing.
- Self-documenting APIs with utoipa (OpenAPI spec generator)

## Available Modules (much more to add later)

| Module | Role |
|---|---|
| `ledger` | Double-entry accounting. |
| `sales` | Invoices, credit notes and payments. |
| `purchases` | Bills and input tax. |
| `tax_sa` | Saudi VAT returns and ZATCA e-invoicing integration (submodule of sales and purchases). |

## Running

You need Rust, Postgres 18, Docker and `just`, I used just to simplify some commands.

1. create an `.env` file with the `DATABASE_URL` variable (you can use local url).

2. start Redis (make sure it's available on port 6379)

   ```bash
   just redis
   ```

3. make the offline query data. Do this again after you change a migration.

   ```bash
   just prepare
   ```

4. run the format check, the lints and the tests.

   ```bash
   just check
   ```

to start the whole system, use this command:

```bash
docker compose up
```

to create demo tenant to test the apis, use this command:

```bash
just demo my-password
```

## Technologies used

| Tech | Reason |
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

I measured each number below on one developer machine, with a release mode build and Postgres 18.

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

Business Source License 1.1. See [LICENSE](LICENSE). A source-available license, turning into Apache 2.0 four years after each major release.

Contributions are welcome
