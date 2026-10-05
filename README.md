# graphmail-bridge

`graphmail-bridge` is a small Rust daemon that exposes a Microsoft 365 mailbox
as local IMAP and SMTP services. It talks to Microsoft Graph over HTTPS and lets
an ordinary Linux mail client connect to `127.0.0.1`.

This is an experimental 0.1 implementation, not yet a drop-in replacement for
every DavMail feature. The initial goal is useful mail support with one binary,
guided OAuth setup, no Java runtime, and a native systemd user service.

## What works

- Microsoft device-code login and automatic refresh-token renewal, with no
  redirect URI or embedded browser
- A zero-registration Microsoft Office compatibility profile plus a conventional
  custom Entra application profile
- Desktop keyring storage by default; an explicit mode-`0600` file fallback
- Recursive folders and special-use folder hints
- A local SQLite index of every message in every folder, kept current with
  Microsoft Graph delta queries, so the whole mailbox history is visible over
  IMAP without re-listing folders on each poll
- IMAP select/status, stable UIDs, `\Seen`/`\Flagged`/`\Deleted` flags, and
  Outlook's pin-to-top as a `$Pinned` keyword that works in both directions,
  server-side SEARCH (FROM/TO/CC/BCC/SUBJECT/TEXT/BODY, dates, sizes,
  Message-ID, NOT/OR, KEYWORD `$Pinned`) answered from the local index,
  MIME/header/body fetch
  with an on-disk body cache, append, copy, move, delete/expunge, and NOOP/IDLE
  updates pushed by the sync task
- Modified UTF-7 mailbox names for clients that do not enable `UTF8=ACCEPT`,
  and locale-independent special-use hints from Graph's well-known folders
- Authenticated SMTP submission of complete MIME messages
- A loopback HTTP endpoint serving Outlook profile pictures, so a mail client
  can show sender avatars from the tenant directory and personal contacts
- Multiple accounts, selected by the IMAP/SMTP username
- A hardened `systemd --user` service generated from the installed binary path
- A SQLite UID cache using Graph immutable message IDs
- Read-only JSON calendar export for a requested UTC window

## Microsoft authentication profiles

The default `microsoft-office` compatibility profile uses Microsoft's public
Office client ID and the v1 resource-based device flow to request a token for
Microsoft Graph. This reproduces the important part of the DavMail workaround
without Java, an embedded webview, or a redirect URI. It can avoid a third-party
application consent prompt when a tenant has already authorized Microsoft
Office to access Graph.

This mode is transparent about its tradeoff: Microsoft and tenant sign-in logs
identify the client as **Microsoft Office**, even though this bridge initiated
the flow. It is a compatibility technique, not a supported identity assigned to
this project. It does not bypass Conditional Access, device-compliance rules,
Exchange authorization, token revocation, or Graph permission enforcement, and
Microsoft can change the first-party registration at any time.

The `custom-entra` profile retains the conventional v2 OAuth flow for users who
have an approved public-client application. That app needs delegated
`Mail.ReadWrite`, `Mail.Send`, `User.Read`, and `Calendars.Read` permissions.

Microsoft references:

- [Device authorization grant](https://learn.microsoft.com/entra/identity-platform/v2-oauth2-device-code)
- [Conditional Access targets resources, not public clients](https://learn.microsoft.com/entra/identity/conditional-access/concept-conditional-access-cloud-apps)
- [Enable public client flows](https://learn.microsoft.com/entra/identity-platform/scenario-desktop-app-registration)
- [Microsoft Graph permissions](https://learn.microsoft.com/graph/permissions-reference)
- [Outlook immutable IDs](https://learn.microsoft.com/graph/outlook-immutable-id)
- [DavMail maintainer's approved-client workaround](https://github.com/mguessan/davmail/issues/71)

## Build and install

Rust 1.88 or newer is required.

```console
git clone YOUR-REPOSITORY-URL graphmail-bridge
cd graphmail-bridge
cargo test
cargo install --locked --path .
```

The last command installs `graphmail-bridge` in Cargo's binary directory,
normally `~/.cargo/bin`.

`./install.sh` is the user-local alternative. It builds the release binary,
installs it in `~/.local/bin`, and, once an account is configured, refreshes
and restarts the `systemd --user` service from that installed path. It needs no
root privileges and no Cargo binary directory on `PATH`.

## Setup

The default setup needs no application registration or client ID:

```console
graphmail-bridge setup
graphmail-bridge doctor
graphmail-bridge install-service
```

Use `graphmail-bridge doctor --refresh` to test both refresh-token renewal and
live Graph mailbox access.

Export expanded calendar occurrences for a UTC window with the same account
and credential used by the mail bridge:

```console
graphmail-bridge calendar-events \
  --start 2026-09-01T00:00:00Z \
  --end 2026-10-01T00:00:00Z
```

Commands that act on a single account (`login`, `client-config`,
`calendar-events`) take an optional account name or email. With one account
configured they use it; with several they ask which one, or name it directly:
`graphmail-bridge client-config work`.

`setup` asks for a short account name and email address, then opens Microsoft's
device-login page and prints its short code. For a headless machine without a
Secret Service-compatible keyring, use `graphmail-bridge setup --file-secrets`;
this stores tokens in `~/.local/share/graphmail-bridge/secrets.json` with mode
0600.

The fully specified compatibility form is:

```console
graphmail-bridge setup \
  --name work \
  --email you@example.com \
  --auth-profile microsoft-office \
  --tenant common
```

To use your own Entra registration, enable public client flows on that app, add
the delegated Graph permissions above, and run:

```console
graphmail-bridge setup \
  --name work \
  --email you@example.com \
  --auth-profile custom-entra \
  --client-id 00000000-0000-0000-0000-000000000000 \
  --tenant organizations
```

A client secret is neither required nor safe for either profile.

## Local index and sync

`serve` runs one background sync task per account. On first start it lists
every folder in full through Graph delta queries (a few hundred requests for a
50k-message mailbox, spaced by `sync.page_delay_ms`), then only fetches changes:
one small request per folder per poll, plus one for the folder's pinned set,
which delta responses cannot carry. Metadata (subject, sender, recipients,
dates, flags, pin state, preview, size) for every message lives in
`~/.local/share/graphmail-bridge/cache.sqlite3`, roughly 4 KB per message
(about 150 MB for a 35k-message mailbox).
Message bodies are downloaded on first open and kept in a size-capped cache.

```toml
[sync]
inbox_poll_secs = 60       # Inbox, Sent, Drafts
folder_poll_secs = 600     # everything else, and the folder list itself
page_size = 300            # messages per delta page (1..=500)
page_delay_ms = 250        # pause between pages during the initial sync
body_cache_max_mb = 2048   # LRU cap for cached MIME bodies
download_bodies = false    # prefetch all bodies (newest first) once indexed
```

`graphmail-bridge sync-status` shows each folder's progress, local and remote
counts, the last successful poll, and body-cache usage. If Graph reports a
delta token as expired, the folder is re-listed automatically; UIDs are
preserved because Graph immutable IDs are used throughout.

## Mail-client settings

Run `graphmail-bridge client-config` to print the generated local bridge
password and exact ports. The defaults are:

| Setting | Value |
| --- | --- |
| Username | Microsoft 365 email address or short account name |
| Password | Generated bridge password from `client-config` |
| IMAP server | `127.0.0.1:1143`, no transport security |
| SMTP server | `127.0.0.1:1025`, no transport security, password auth |
| Profile photos | `http://127.0.0.1:1180/photo?address=<email>`, HTTP Basic auth |

### Profile photos

The photo endpoint answers `GET /photo?address=<email>` with the same username
and bridge password as IMAP, sent as HTTP Basic credentials. The account's own
address maps to `/me/photo`; any other address is looked up as a user in the
tenant directory and then as a personal contact. `200` carries the image with
its content type, `404` means nobody has a picture, and `502` means Graph could
not be reached. Answers are cached in memory for a day (hits) or six hours
(misses). `server.photo_port` changes the port. This needs no additional Graph
permission beyond what the mail scopes already grant; where a tenant does not
allow reading other users' photos the endpoint simply answers `404`.

```console
curl -u you@example.com:BRIDGE_PASSWORD \
  "http://127.0.0.1:1180/photo?address=colleague@example.com" -o photo.jpg
```

The lack of transport TLS is intentional only because the listeners are
strictly loopback-only. The program refuses a non-loopback bind address.

## Service operation

The installer writes `~/.config/systemd/user/graphmail-bridge.service` and runs
the ordinary user-service commands. It never needs root privileges.

```console
systemctl --user status graphmail-bridge
journalctl --user -u graphmail-bridge -f
systemctl --user restart graphmail-bridge
```

To remove only the service while preserving accounts and tokens:

```console
graphmail-bridge uninstall-service
```

Configuration is in `~/.config/graphmail-bridge/config.toml`; cache data is in
`~/.local/share/graphmail-bridge/`. Run `graphmail-bridge login` when an
administrator revokes consent or a refresh token expires.

## Current limitations

- IMAP support is deliberately a practical subset, not a complete standards
  conformance claim. SEARCH keys the bridge cannot evaluate faithfully
  (`HEADER` on arbitrary fields, keywords, literals) are refused rather than
  guessed, and `BODY`/`TEXT` only see bodies that are in the local cache. IDLE
  reacts to the sync task's polls (every `sync.inbox_poll_secs`), not to push
  notifications from Graph.
- Messages uploaded with APPEND become Outlook drafts, because that is how
  Graph creates MIME messages. APPEND to the sent folder is accepted but
  ignored, since Graph already files a copy of everything sent via SMTP.
- Until a folder's first sync completes, SELECT serves a live listing of its
  newest 500 messages and SEARCH is unavailable for it. Check progress with
  `graphmail-bridge sync-status`. (`server.message_limit` is deprecated and only
  sizes that bootstrap listing.)
- Shared/delegated mailboxes, calendar mutation/CalDAV, contacts, Exchange
  categories, S/MIME authoring, and NTLM/EWS emulation are not implemented.
- The service needs network access and, with the default credential backend, an
  unlocked desktop keyring.
- The `microsoft-office` compatibility profile depends on a Microsoft-owned app
  registration that this project cannot control. A tenant or Microsoft may
  reject it now or later.

These limitations are why the version is `0.1.0`. Test against a non-critical
mailbox before trusting destructive IMAP operations.

## Design notes

The project was informed by studying DavMail's GPL-2.0-or-later source,
especially its separation between local protocols, authentication, and its
Graph-backed Exchange session. Its Office compatibility profile also follows
DavMail's distinction between v2 scope-based OAuth for a custom client and v1
resource-based OAuth for the public Office client. This implementation is new
Rust code and is distributed under the same compatible GPL-2.0-or-later
license. It uses Graph's MIME endpoints so message content is not reconstructed
from lossy JSON fields.

Run the development checks with:

```console
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
