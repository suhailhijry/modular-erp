# What the modules do

Tenants switch these on individually, and each one depends only on the modules
above it.

## ledger

Double-entry accounting: accounts, journal entries, fiscal periods and VAT rates.

A journal entry has to sum to zero, and the type carrying its lines revalidates
when it's read back, so a bad migration surfaces as a decode error instead of a
trial balance that quietly doesn't balance.

It ships with ready-made charts of accounts in Arabic and English. Installing a
chart in English and asking a Saudi accountant to rename every account would be
technically correct and completely useless.

## sales

Invoices, credit notes and the money customers pay.

An invoice is addressed by a key the client chooses, so sending the same one
twice does nothing twice. It freezes the buyer's name, tax number and address as
they were on the day it was issued, because a tax invoice is a legal document
and last year's copy mustn't change when somebody edits a customer record.

It also answers who owes money, how much, and how overdue they are, bucketed by
age and grouped by customer and currency.

## purchases

Bills, the money you pay suppliers, and input tax. The same supplier invoice
can't be recorded twice.

## tax_sa

Saudi VAT and ZATCA electronic invoicing.

It builds the VAT return by netting output tax against input tax, which is why
it sits above both `sales` and `purchases`. Each invoice gets signed, given a
sequential counter and the hash of the invoice before it, then submitted to
ZATCA.

The Saudi rate arrives as seeded data when a tenant enables the module, kept
separate from the schema because a tenant's data and a tenant's tables are
different things.

## How modules learn about each other

By subscribing to the log. `tax_sa` finds out an invoice was issued by reading
events, and `sales` has no idea Saudi Arabia exists. Adding a country module
changes nothing in the modules it depends on.
