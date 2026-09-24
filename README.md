# Crate Digger service

The central service for [Crate Digger](https://github.com/timini/crate-digger): a shared catalogue of track metadata, fingerprints and audio embeddings that users choose to contribute, and private backups of each user's ratings, seeds and playlists.

The desktop app works fully without it. Sharing and backup are separate opt-in settings.

**Status: built and tested, not yet deployed.** Design: [decision 0003](https://github.com/timini/crate-digger/blob/main/docs/decisions/0003-central-service.md).

## Design in brief

- Rust (axum) on Google Cloud Run.
- Firestore for shared records and contributions; Cloud Storage for backup snapshots.
- Google sign-in: the app sends a Google ID token, the service verifies it and uses the account's subject id. Backups are only ever visible to their owner.
- Request and response types come from the `cd-protocol` crate in the app repository, so both sides share one versioned contract.
- Contributions are treated as untrusted: schema, model version, embedding dimension and size are checked, writes are idempotent, and conflicting contributions are kept rather than overwritten.

## API (v1)

All routes except `GET /healthz` need `Authorization: Bearer <Google ID token>`.

| Route | Purpose |
| --- | --- |
| `POST /v1/contributions` | Submit up to 100 contributions; each is acknowledged as accepted, duplicate or rejected with a reason |
| `POST /v1/lookup` | Catalogue entries for recordings by fingerprint hash or external id |
| `POST /v1/features` | The embedding contributors agree on for a recording and analysis version |
| `POST /v1/changes` | Catalogue changes after a cursor |
| `GET /v1/backups`, `PUT/GET/DELETE /v1/backups/{id}` | The caller's own backups only |

## Development

```sh
cargo test                        # API and storage tests with the in-memory store
firebase emulators:exec --only firestore --project demo-crate-digger "cargo test --test store"
STORE=memory GOOGLE_CLIENT_IDS=<client id> cargo run    # run locally
```

Deployment is by hand with `scripts/deploy.sh` (Cloud Run, Firestore, a private bucket and a least-privilege service account). `firestore.rules` denies all direct client access.
