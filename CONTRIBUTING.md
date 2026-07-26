# Contributing

Thanks for looking at Aviary. It is a desktop mail client with a lot of surface
— three backends, an HTML renderer, a block editor — so this page is mostly
about the few rules that keep that surface from drifting.

## Getting a build

The build needs system libraries (Vulkan, Wayland/X11, fontconfig, freetype,
dbus). `shell.nix` declares them and every `just` recipe wraps its command in
`nix-shell`, so the recipes are the supported entry point:

```sh
just            # debug build, runs the app
just check      # cargo check --all-targets
just test       # unit tests
just clippy     # cargo clippy --all-targets -- -D warnings
just fmt
```

Running `cargo` directly works only if those libraries are already in scope.
The first build compiles Stylo and Blitz and takes a while; after that the
`[profile.dev.package.*]` block in `Cargo.toml` keeps the rendering hot path at
`opt-level = 3` so debug builds stay usable.

## The gate

CI runs exactly four things, and they are what a pull request has to pass:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings` — no warnings, no `#[allow]`
  added to silence one without a comment saying why
- `cargo test`
- `assets/i18n/fr.json` and `en.json` expose the same keys

Run `just fmt && just clippy && just test` before opening the pull request; the
i18n check is a Python snippet in `.github/workflows/ci.yml` you can read in
ten seconds.

## Rules that are not obvious from the code

- **The UI never does I/O.** Adding an async operation means adding a `Cmd`
  variant, dispatching it in `runtime::run`, and emitting `Evt`s back — never
  calling the network from the gpui thread. See "Two-thread Cmd/Evt loop" in
  `CLAUDE.md`.
- **Backends hide behind `providers::Session`.** Runtime code and the UI never
  call `graph::*` / `gmail::*` / `imap::*` directly. A feature that only one
  backend can do still goes through the `Session` surface, failing with a
  translated error on the others.
- **Every user-visible string goes through `tr!`**, with both catalogs updated
  in the same commit. That includes error messages, placeholders and generated
  reply text.
- **`patches/` is not where you develop.** Those four crates are minimal
  buildable copies of published ones, pinned through `[patch.crates-io]`;
  `patches/README.md` lists the exact versions and the modified files. Bumping
  one means re-copying upstream and re-applying those changes.
- **When a change is architectural, update `CLAUDE.md` in the same commit.**
  It documents *why* things are the way they are; a decision explained nowhere
  gets undone six months later.

## Test data and secrets

Never put real-world or user-supplied data in sources, tests, fixtures,
snapshots, logs, comments or commit messages — no personal names, email
addresses, message contents, tenant or account identifiers, company names,
domains or credentials. Use synthetic placeholders (`Contact A`,
`Organisation de test`) and reserved domains (`example.com`, `example.test`,
`example.invalid`). A fixture derived from a real email gets reduced to the
structure the test needs and anonymized before it is committed.

No OAuth secret belongs in the sources. Google's client id and secret are
injected at build time from `AVIARY_GOOGLE_CLIENT_ID` /
`AVIARY_GOOGLE_CLIENT_SECRET` (see the README) precisely so that no literal
ends up in a public repository.

## Reporting a problem

For a bug, say which provider (Microsoft 365, Gmail, IMAP), which distribution
and desktop session (Wayland or X11), and how you installed Aviary. The in-app
log — **Préférences → Logs** — has a copy button, and it is usually the fastest
way to make a rendering or synchronisation problem reproducible.

If you think you have found a security problem — anything touching OAuth
tokens, the credential store, or the rendering of untrusted HTML — please
report it privately through GitHub's "Report a vulnerability" form on the
repository's Security tab rather than in a public issue.

## License

Aviary is Apache-2.0. By opening a pull request you agree that your
contribution is licensed under those terms.
