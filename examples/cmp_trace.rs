// Inspect a JM ldecod trace (docs/libmvc-poc.md § Validation): parse
// trace_dec.txt, summarise the syntax-element stream, and dump the first
// macroblock's elements — the per-element ground truth the libmvc
// macroblock decoder is built against.
//
// Generate the trace first:
//   scripts/build-ldecod-trace.sh                 # one-time
//   cargo run --release --example extract_mvcc_mkv -- clip.mkv s.h264
//   (cd /tmp && ldecod-trace -f dec.cfg)          # writes trace_dec.txt
//   cargo run --release --example cmp_trace -- /tmp/trace_dec.txt
//
// When the libmvc MB decoder exists, it will emit its own (name, value)
// sequence and `trace::first_divergence` will pinpoint the first mismatch.

use std::collections::BTreeMap;

use ripsaw::mvc::trace::{macroblock_elements, parse_trace};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: cmp_trace <trace_dec.txt>"))?;
    let text = std::fs::read_to_string(&path)?;
    let elems = parse_trace(&text);
    let mb = macroblock_elements(&elems);

    eprintln!(
        "parsed {} syntax elements ({} header, {} macroblock-layer)",
        elems.len(),
        elems.len() - mb.len(),
        mb.len()
    );

    // Histogram of macroblock-element names.
    let mut hist: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &mb {
        *hist.entry(e.name.as_str()).or_default() += 1;
    }
    eprintln!("\nmacroblock-element histogram:");
    for (name, n) in &hist {
        eprintln!("  {n:>8}  {name}");
    }

    // First macroblock: from the first `mb_type` up to (but not including)
    // the next one.
    let first = mb.iter().position(|e| e.name == "mb_type");
    if let Some(start) = first {
        let next = mb[start + 1..]
            .iter()
            .position(|e| e.name == "mb_type")
            .map(|p| start + 1 + p)
            .unwrap_or(mb.len());
        eprintln!("\nfirst macroblock ({} elements):", next - start);
        for e in &mb[start..next] {
            eprintln!("  {:<28} {}", e.name, e.value);
        }
    }
    Ok(())
}
