# Security

How TIDE handles your TIDAL account, what it stores, and what it does not
protect against. This describes the code as it is; where something is a
limitation, it says so.

## What counts as a secret

| Secret | Where it lives | Notes |
| --- | --- | --- |
| OAuth access token and refresh token | `~/Library/Application Support/tidal-rust/session.json` | Lets a holder act as your TIDAL account until revoked |
| Account details returned by TIDAL (user id, e-mail/username, country) | Same file | Stored alongside the tokens by the login library |
| One-time login code (device code, or the address you paste for the Max-quality login) | Memory only | Exchanged for tokens over HTTPS, never written to disk |

TIDE never sees or stores your TIDAL password. Both login flows happen on
TIDAL's own pages in your browser.

## How the secrets are protected

- **Owner-only files.** `session.json` is written with mode `0600` and its
  directory is set to `0700`, so other user accounts on the Mac cannot read
  it. The file is written to a private temporary file and renamed into
  place, so it is never world-readable, even briefly. The same permissions
  are re-applied every time the session is loaded.
- **Never in the repository.** Tokens are only ever written under
  `~/Library`, outside the source tree. Nothing in this repository contains
  an account credential, and `.gitignore` excludes build output and the
  private design-reference screenshots.
- **Never in logs.** TIDE does not log tokens. The default log level prints
  errors only. With `RUST_LOG=debug` the login library logs request URLs
  and whether a token was attached, not the token itself. Short-lived signed
  audio URLs can appear in debug logs, so do not share those logs.
- **HTTPS only.** Every request to TIDAL goes over TLS.
- **Replace on success only.** Starting a new login does not touch the
  existing session; it is replaced only after the new one has been obtained
  and saved.
- **Log out.** Logging out asks TIDAL to invalidate the token, deletes
  `session.json`, and clears the cached account data (home feed, playlists,
  favourites, lyrics). App preferences are kept.

## What else is stored on this Mac

| Data | Location | Protection |
| --- | --- | --- |
| Library metadata, listening history, lyrics, preferences | `~/Library/Application Support/tidal-rust/library.db` | Mode `0600`, no tokens |
| Audio you have played | `~/Library/Caches/tidal-rust/audio/` | Unencrypted audio files, no tokens |
| Artwork | `~/Library/Caches/tidal-rust/images/` | Public images |

The audio cache can be emptied from Settings.

## Network behaviour

TIDE contacts TIDAL only: the API (`api.tidal.com`, `tidal.com`,
`openapi.tidal.com`), login (`auth.tidal.com`, `login.tidal.com`), artwork
(`resources.tidal.com`, `images.tidal.com`) and the audio CDN hosts named in
TIDAL's stream manifests. There is no telemetry, analytics, crash reporting
or update check, and no third-party service receives anything.

Actions that change your TIDAL account are limited to the favourite (heart)
button, which adds or removes a track from your favourites.

## Limitations you should know about

- **No Keychain yet.** The tokens are protected by file permissions, not by
  the macOS Keychain. Any program running as your macOS user can read
  `session.json`. Moving the tokens into the Keychain is the main planned
  improvement.
- **No encryption at rest** beyond what FileVault provides for the disk.
- **Third-party code handles the tokens.** Login and token refresh are
  implemented by the [`tidlers`](https://crates.io/crates/tidlers) crate,
  which also supplies the TIDAL client identifiers. Dependency versions are
  pinned in `Cargo.lock`.
- **Unsigned build.** TIDE is built from source and is not code-signed or
  notarised.
- **Unofficial client.** TIDE is not affiliated with TIDAL; using it is
  subject to TIDAL's terms for your account.

## If you think a token leaked

1. Log out in TIDE (this revokes the token with TIDAL).
2. In your TIDAL account settings, sign out of other devices/sessions.
3. Change your TIDAL password.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository
(Security → Report a vulnerability) rather than a public issue.
