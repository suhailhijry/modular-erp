# Introduction

This is an ERP backend written in Rust, storing everything in Postgres. It
serves many companies from a single deployment, and each company gets its own
database.

## What it holds

A company using this system keeps its accounts here, writes its invoices here,
and records what it buys and sells. In Saudi Arabia it also files its VAT return
and submits every invoice to ZATCA from the same place.

## What makes it unusual

Most systems store the current state of things: a row per invoice, updated in
place as the invoice gets paid or cancelled. This one stores what happened
instead. An invoice was issued, a payment arrived, a credit note cancelled it.
The tables you actually query are built from that history, and they can be
rebuilt from it whenever we need to.

The history itself can't be edited. The database refuses updates and deletes on
the log, which means a company that has to show an auditor what happened in
March 2027 can still show exactly that, because nothing since then had the
ability to change it.

## Where to start

Read [Words this system uses](./words.md) first, since the rest of the book
assumes those meanings.

If you're going to operate the system, the chapters you need are
[On your computer](./local.md), [Backup and restore](./restore.md) and
[Upgrades](./upgrades.md).

If you're evaluating it, read [Three ways to deploy](./deployment.md) and
[Why it is built this way](./decisions.md).

If you're going to write code for it, read [The eight laws](./laws.md) before
you start. A change that breaks one of them fails the build, so knowing them in
advance saves you the failure.
