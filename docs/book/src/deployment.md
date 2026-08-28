# Three ways to deploy

All three run the same program, differing only in who operates it and where the
data sits.

| | Who operates it | Where the data is |
|---|---|---|
| Shared fleet | We do | Our clusters, one database per tenant |
| Dedicated instance | We do | A machine for that tenant alone |
| Customer hosted | The customer does | Their own machines or cloud account |

The third one is BYOC, or bring your own cloud. The part holding a customer's
data runs inside the customer's account, and the part managing versions and
licences stays with us. Only management data crosses between the two, never
invoices, ledgers or customer records.

## Tenants call us, we never call tenants

A customer hosting the system opens no inbound port for us. Their deployment
asks for its licence and reports the version it's running, and nothing ever
reaches into their network.

The practical result is that our control plane can go down without stopping a
single tenant, which is the property that makes the self-hosted tier worth
selling at all.

## Why we still share

Putting a tenant on its own machine costs several times more than putting it on
a shared cluster. Against what a company pays for an ERP that difference is
small, but it buys you one more deployment to patch, back up and upgrade, and
that cost is per tenant forever.

So the shared fleet carries the long tail, and dedicated instances go to
customers who ask for one. In Saudi Arabia that usually means a company under
financial regulation, or one that has committed to keeping its data in its own
hands.
