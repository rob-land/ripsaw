# Disc-hash fixtures

Each `*.json` here is a captured TheDiscDB disc record reduced to:

- `expected_hash` — the published `ContentHash` from the disc's
  `disc##.json` in github.com/TheDiscDb/data
- `files` — the ordered list of per-file (`index`, `name`, `size`) tuples
  derived from `HSH:` log lines in the same disc's `disc##.txt`

`tests/disc_hash_fixtures.rs` loads every file in this directory and
asserts that `identify::disc_hash::content_hash(files)` matches
`expected_hash`. See `docs/disc-hash.md` for the algorithm.

To extend the corpus, run the helper checked in at the project root
(`scripts/fetch_disc_hash_fixtures.py` — TODO) or replicate the
algorithm: find a disc record in TheDiscDb/data whose `disc##.txt`
contains `HSH:` lines, capture them in the JSON shape used by the
existing fixtures, and verify locally before committing.
