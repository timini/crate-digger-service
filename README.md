# Crate Digger service

The central service for [Crate Digger](https://github.com/timini/crate-digger): a shared catalogue of track metadata, fingerprints and audio embeddings that users choose to contribute, and private backups of each user's ratings, seeds and playlists.

The desktop app works fully without it. Sharing and backup are separate opt-in settings.

**Status: being planned.** See the plan in the app repository: [milestone 4](https://github.com/timini/crate-digger/blob/milestone-4/docs/milestone-4-plan.md).

## Design in brief

- Rust (axum) on Google Cloud Run.
- Firestore for shared records and contributions; Cloud Storage for backup snapshots.
- Google sign-in: the app sends a Google ID token, the service verifies it and uses the account's subject id. Backups are only ever visible to their owner.
- Request and response types come from the `cd-protocol` crate in the app repository, so both sides share one versioned contract.
- Contributions are treated as untrusted: schema, model version, embedding dimension and size are checked, writes are idempotent, and conflicting contributions are kept rather than overwritten.
