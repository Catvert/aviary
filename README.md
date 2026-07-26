# Aviary

Desktop email, calendar and kanban client for **Microsoft 365**, **Gmail** and
**IMAP/SMTP**, written in Rust on [gpui](https://www.gpui.rs/) (Zed's UI
framework) with [gpui-component](https://github.com/longbridge/gpui-component)
widgets. Linux is the primary target; macOS and Windows build and run, with the
gaps listed under [Platforms](#platforms).

- **Mail** — unified or per-account inbox, virtualized message list, folder
  tree, tags, search, conversation threading, sender history.
- **Reader** — faithful HTML rendering through the [Blitz](https://github.com/DioxusLabs/blitz)
  engine (Stylo + Taffy + parley) with text selection and clickable links, plus
  Markdown and source views.
- **Composer** — Notion-style block editor (headings, lists, quotes, code,
  tables, inline images with resize handles), signatures, templates, inline
  replies and detachable windows.
- **Calendar** — continuously scrolling week grid, list view, event editing,
  meeting-request responses, subscribed iCalendar feeds.
- **Kanban** — one column per tag, native drag and drop.
- **Contacts**, offline mail cache (SQLite), durable outbox, French/English UI,
  optional offline spell checking and LanguageTool integration.

## Platforms

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Mail, calendar, kanban, reader, composer | yes | yes | yes |
| Single instance (no two writers on the session and databases) | Unix socket | Unix socket | named pipe |
| `mailto:` handler | `.desktop` entry | `Info.plist`, delivered as an Apple Event | `register-mailto.ps1` |
| Notifications | yes | needs the `.app` bundle | yes |
| System tray | yes | — | — |
| Credentials at rest | `0600` + OS keyring | `0600` + Keychain | per-user ACL + Credential Manager |
| Released binaries | tarball | unsigned `.app` zip | unsigned zip |

Linux is what every release is tested on. The other two are built by CI and
attached to releases, but unsigned: macOS refuses an unsigned bundle downloaded
from the internet until you clear its quarantine attribute
(`xattr -d com.apple.quarantine Aviary.app`), and Windows SmartScreen warns
before running the executable. The gaps above are tracked, not hidden — the
tray in particular is a freedesktop `StatusNotifierItem`, so its switch does
not appear in Preferences where nothing implements it.

## Installing

Linux x86_64 builds are attached to each [release](https://github.com/Catvert/aviary/releases):
unpack the tarball and run `./install.sh`, which places the binary, desktop
entry and icon under `~/.local` (set `PREFIX=` for somewhere else). The usual
desktop runtime libraries are expected to be present — Vulkan loader,
Wayland or X11, fontconfig, freetype, D-Bus.

On **macOS**, unpack the zip and drag `Aviary.app` into `/Applications`. The
bundle is what gives Aviary a bundle identifier — without one macOS refuses to
post notifications — and what registers it for `mailto:` links.

On **Windows**, unpack the zip anywhere and run `aviary.exe`. To be offered as
the mail client for `mailto:` links, run the bundled script once:

```powershell
powershell -ExecutionPolicy Bypass -File register-mailto.ps1
```

It writes under `HKCU` only (no administrator rights), and `-Unregister`
undoes it. A Vulkan-capable GPU driver is required, as on Linux.

With Nix, the flake builds and wraps all of that itself:

```sh
nix run github:Catvert/aviary          # try it
nix profile install github:Catvert/aviary
```

Both pull from a [Cachix](https://cachix.org) binary cache, so the Stylo/Blitz
graph is downloaded rather than compiled — the difference between a minute and
the better part of an hour. The flake declares it, and Nix will ask once
whether to trust the setting; answering no just builds locally. To accept it
without being asked, either pass `--accept-flake-config` or configure it
permanently:

```sh
cachix use catvert
```

The cache holds what CI builds from `main`. It never contains a release
binary: those are built outside Nix, and are the only ones carrying a bundled
Google OAuth registration.

## Building

The build needs system libraries (Vulkan, Wayland/X11, fontconfig, freetype,
dbus). `shell.nix` provides them, and every `just` recipe already wraps its
command in `nix-shell` — so the recipes are the supported entry point:

```sh
just            # debug build, runs the app
just release    # cargo run --release
just build      # release binary
just check      # cargo check --all-targets
just test       # unit tests
just clippy     # cargo clippy --all-targets -- -D warnings
just fmt
just logout     # wipe pending and per-account credentials, forcing re-auth
```

Running `cargo` directly works only if the libraries from `shell.nix` are
already in scope (`nix-shell` first, or an equivalent setup on another distro).
Built with Rust 1.97.

On macOS and Windows there is nothing to provide: `cargo build` works with a
stock toolchain, and the `just` recipes fall back to plain cargo when
`nix-shell` is absent. `just bundle-macos` assembles `Aviary.app` around the
release binary.

`clippy` must stay clean with `-D warnings`; that and `just test` are the gate
for any change.

### Patched dependencies

`patches/` holds four crates pinned through `[patch.crates-io]`. They are
minimal buildable copies of upstream, not forks to develop in — see
`patches/README.md` for the exact versions and the six modified files. The two
non-obvious ones:

- **`stylo_derive`** — gpui enables `log/kv_serde`, which pulls `serde_fmt` into
  the graph; its `impl From<serde_fmt::Error> for fmt::Error` makes the `?` in
  Stylo's `derive(ToCss)` output ambiguous. The vendored copy replaces those `?`
  with explicit `match`es.
- **`fontique`** is a direct dependency purely to select `fontconfig-dlopen`,
  matching the API gpui already selected through font-kit.

Bumping Blitz or Stylo means re-copying the new crate sources and re-applying
those changes.

## Configuration

Everything lives under `~/.config/aviary/` (`directories::ProjectDirs`):

| File | Contents |
| --- | --- |
| `settings.json` | UI settings (`0600`) |
| `session.json` | working session — open tabs, selection (`0600`) |
| `accounts/<id>.json` | per-account OAuth tokens / IMAP server settings (`0600`) |
| `pending_tokens.json` | authentication interrupted before `/me` resolved (`0600`) |

IMAP and SMTP passwords go to the OS credential store (Secret Service, Keychain,
Credential Manager) through `keyring`, never to disk. The mail cache is a
separate SQLite database whose size limit is set in Preferences.

### OAuth registrations

Microsoft's `client_id` is bundled in the sources and needs nothing from you.

**Google's is not in this repository.** Its client id and secret are injected at
build time from `AVIARY_GOOGLE_CLIENT_ID` / `AVIARY_GOOGLE_CLIENT_SECRET`, so
official release binaries carry one and anything you build yourself does not.
Not because the secret is confidential — a native app is a public client under
[RFC 8252](https://www.rfc-editor.org/rfc/rfc8252) and cannot keep one; PKCE is
what secures the flow — but because a `GOCSPX-…` literal in a public repository
gets flagged and revoked, which would sign out every Gmail account at its next
token refresh.

So if you build from source and want Gmail, create a **Desktop app** OAuth
client in the Google Cloud console and paste both halves into **Préférences →
Comptes → Configuration Google**. Aviary asks for both and refuses a half-filled
pair, since your client id combined with someone else's secret only fails later,
in Google's words rather than ours.

One consequence worth knowing before relying on the bundled registration:
`gmail.modify` is a Google **restricted** scope, so until the app clears OAuth
verification (a third-party security audit, renewed annually) it is capped at
100 test users. Your own registration has the same cap until you verify it, and
is exempt while you are the only user of it.

Tenants that block unverified third-party apps can likewise substitute their own
Azure registration under **Préférences → Comptes** (`azure_client_id` /
`azure_tenant`). All these values are persisted onto each account's tokens, so
changing them later does not break accounts already signed in.

## Architecture

`CLAUDE.md` is the detailed map — module layout, the Cmd/Evt threading model,
provider abstraction, and the gpui pitfalls worth knowing before touching the
UI. The short version:

- **The UI never does I/O.** `runtime::spawn` starts a dedicated OS thread
  hosting a current-thread Tokio runtime; the UI sends `Cmd`s and receives
  `Evt`s over unbounded mpsc channels. Adding an async operation means adding a
  `Cmd` variant, dispatching it in `runtime::run`, and emitting `Evt`s back.
- **Backends hide behind `providers::Session`**, an enum pairing one backend
  with its live credentials so a provider/credentials mismatch cannot be
  expressed. Runtime code calls the `Session` surface only, never
  `graph::*` / `gmail::*` / `imap::*` directly.
- **Mutations are durable.** Sends, moves, flags and read-state changes go
  through an SQLite-backed outbox that retries with exponential backoff and
  deliberately refuses to replay a send that may already have been delivered.
- **All user-visible strings go through `tr!`** (`assets/i18n/{fr,en}.json`);
  both catalogs must stay key- and placeholder-compatible.

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) covers the build, the four checks CI runs,
and the handful of rules that are not obvious from the sources — the UI does no
I/O, backends stay behind `providers::Session`, user-visible strings go through
`tr!` with both catalogs updated, and `patches/` is not a place to develop.

## License

Apache-2.0 — see [`LICENSE`](LICENSE). Bundled fonts, dictionaries, icons and
the vendored crates under `patches/` keep their own licenses, listed in
[`NOTICE`](NOTICE).
