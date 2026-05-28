# Disc content hash (TheDiscDB)

TheDiscDB identifies a physical disc by a deterministic hash computed
from the *sizes* of the disc's content files (not their contents). This
is cheap to compute, stable across copies, and not affected by mastering
metadata that may vary.

## Algorithm

Confirmed against
[`HashingExtensions.cs`](https://github.com/TheDiscDb/data/blob/main/tools/ImportBuddy/source/ImportBuddy/TheDiscDb.Core/DiscHash/HashingExtensions.cs):

```csharp
public static string CalculateHash(this IEnumerable<FileHashInfo> files)
{
    HashAlgorithm hash = MD5.Create();
    foreach (var file in files)
    {
        byte[] fileSizeBytes = BitConverter.GetBytes(file.Size);   // long, little-endian on .NET
        hash.TransformBlock(fileSizeBytes, 0, fileSizeBytes.Length,
                            new byte[fileSizeBytes.Length], 0);
    }
    hash.TransformFinalBlock(Array.Empty<byte>(), 0, 0);
    return BitConverter.ToString(hash.Hash).Replace("-", "");
}
```

In plain language:

1. Take the ordered list of `FileHashInfo` records (each carries `Index`,
   `Name`, `CreationTime`, `Size`).
2. For each file, append `Size` as 8 little-endian bytes (`long`) to an
   MD5 state.
3. Finalise. Format the 16-byte digest as uppercase hex with no
   separators — 32 hex characters.

The hash depends *only* on size bytes; filenames and timestamps are not
mixed in.

## What "files" means

Confirmed against `ImportBuddy/DiskContentHash.cs::HashMediaDisc`:

- **Blu-ray / UHD**: every file matching `*.m2ts` under `BDMV/STREAM/`,
  sorted lexicographically by filename (`OrderBy(e => e.Name)`). The
  enumeration is `Directory.GetFiles(path, "*.m2ts")` — no companion
  files, no recursion.
- **DVD**: every file under `VIDEO_TS/` with no extension filter
  (`Directory.GetFiles(path, "*")` — so `VIDEO_TS.IFO`, `VIDEO_TS.BUP`,
  `VIDEO_TS.VOB`, all `VTS_*.IFO/BUP/VOB`), sorted lexicographically by
  filename.

`Index` in the resulting `FileHashInfo` is assigned in iteration order
(which is already sorted-by-name), so `index` and "sort by name" are
equivalent in fixtures.

Validated against 5 fixtures under `tests/fixtures/disc_hash/` covering
UHD, Blu-ray, and DVD across both movie and series releases. See
`tests/disc_hash_fixtures.rs`.

## Rust shape

```rust
// src/identify/disc_hash.rs
pub struct DiscFile {
    pub index: u32,
    pub name: String,
    pub size: u64,
}

pub fn content_hash(files: &[DiscFile]) -> String {
    use md5::{Md5, Digest};
    let mut hasher = Md5::new();
    for f in files {
        hasher.update(f.size.to_le_bytes());
    }
    format!("{:X}", hasher.finalize())  // uppercase hex, no separators
}
```

## Validation

Implemented:

1. Fixture corpus under `tests/fixtures/disc_hash/` (currently 5 discs:
   UHD, Blu-ray, DVD across movies and series). Each captures the
   `expected_hash` (`ContentHash` from `disc##.json`) and the per-file
   `(index, name, size)` list (from `HSH:` lines in `disc##.txt`).
2. Cargo integration test (`tests/disc_hash_fixtures.rs`) loads every
   fixture and asserts that `content_hash(files)` matches.
3. Helper script `scripts/fetch_disc_hash_fixtures.py` reproduces the
   corpus from the upstream repo and verifies each disc locally before
   writing the fixture file.

## Risks

- **Byte order**: `.NET` `BitConverter.GetBytes` is host-endian. On x64
  this is little-endian, which is what our `to_le_bytes()` produces. If
  TheDiscDB ever runs ImportBuddy on a big-endian host, hashes diverge —
  but no such host exists in their toolchain in practice.
- **File enumeration order**: the single highest-risk reproducibility
  failure mode. Encode the rule (which subdirs, which extensions, sort
  by name) explicitly in code and tests; do not rely on filesystem
  iteration order.
- **Trailing/zero-length files**: include or exclude? ImportBuddy's
  behaviour is the source of truth; mirror it.
