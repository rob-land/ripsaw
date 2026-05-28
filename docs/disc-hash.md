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

TheDiscDB's `ImportBuddy` enumerates the disc's main payload files in a
deterministic order:

- Blu-ray / UHD: every `*.m2ts` file under `BDMV/STREAM/`, sorted by
  filename (which is numeric), then any companion files the importer
  emits for that disc type.
- DVD: every `VTS_##_#.VOB` and `VIDEO_TS.VOB` file under `VIDEO_TS/`,
  sorted by filename.

Our Rust re-implementation must match TheDiscDB's ordering exactly. The
canonical reference is `ImportBuddy`'s file enumeration code; we will
mirror its sort order in `src/identify/disc_hash.rs` and validate against
captured fixtures (a small set of known-good disc → expected-hash pairs).

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

## Validation plan

1. Build a fixture corpus: pull 5–10 known disc records from
   `github.com/TheDiscDb/data` whose `contentHash` field is published and
   whose file size list is included in the same record. Store the
   `(sizes[], expected_hash)` pairs as `tests/fixtures/disc_hash/*.json`.
2. Run `content_hash(sizes)` against each and assert equality.
3. Wire this as a Cargo test so any future refactor catches a regression
   before it ships.

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
