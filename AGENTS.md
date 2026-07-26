# AGENTS.md

**The project guide is [`CLAUDE.md`](CLAUDE.md)** — read it before touching the
code. It is the single maintained document: module layout, the two-thread
Cmd/Evt model, the provider abstraction, the local cache and durable outbox,
and the gpui pitfalls that are not visible from the sources.

This file used to be a second copy of it for another assistant, and the two had
already drifted; a pointer cannot drift.

Three rules worth stating here, because getting them wrong is expensive:

- **The UI never does I/O.** Add a `Cmd` variant, dispatch it in
  `runtime::run`, emit `Evt`s back — never call the network from the gpui
  thread.
- **Never copy real-world or user-supplied data** into sources, tests,
  fixtures, logs, comments or commit messages: no personal names, addresses,
  message contents, tenant identifiers, company names, domains or credentials.
  Use synthetic placeholders (`Contact A`) and reserved domains
  (`example.com`, `example.test`, `example.invalid`).
- **`just clippy` (with `-D warnings`) and `just test` must stay green**, and
  every user-visible string goes through `tr!` with both `assets/i18n/fr.json`
  and `en.json` updated. See [`CONTRIBUTING.md`](CONTRIBUTING.md).
