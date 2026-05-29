// Quick diagnostic: count NAL units per `nal_unit_type` in an Annex B
// H.264 byte stream. Useful for confirming whether an MKV-extracted
// stream actually carries MVC NAL types (14, 15, 20).
//
//   cargo run --example scan_nals -- /tmp/foo.h264

use std::collections::BTreeMap;
use std::path::PathBuf;

use threedrip::mvc::annexb::NalSplitter;
use threedrip::mvc::nal::parse_nal_unit_header;

fn type_name(t: u8) -> &'static str {
    match t {
        1 => "Slice (non-IDR)",
        2 => "Slice data partition A",
        3 => "Slice data partition B",
        4 => "Slice data partition C",
        5 => "Slice (IDR)",
        6 => "SEI",
        7 => "SPS",
        8 => "PPS",
        9 => "AUD",
        10 => "EoSeq",
        11 => "EoStream",
        12 => "Filler",
        13 => "SPS extension",
        14 => "Prefix NAL (MVC)",
        15 => "Subset SPS (MVC)",
        16 => "Depth parameter set",
        17 => "Reserved",
        18 => "Reserved",
        19 => "Slice layer (aux)",
        20 => "Slice layer extension (MVC)",
        21 => "View component scalable extension",
        _ => "?",
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: scan_nals <h264_annex_b_file>"))?;
    let data = std::fs::read(PathBuf::from(&path))?;
    let mut counts: BTreeMap<u8, u64> = BTreeMap::new();
    let mut first_seen: BTreeMap<u8, usize> = BTreeMap::new();
    let mut total: u64 = 0;
    for (i, nal) in NalSplitter::new(&data).enumerate() {
        if nal.is_empty() {
            continue;
        }
        let (header, _) = match parse_nal_unit_header(nal) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let t = header.nal_unit_type;
        *counts.entry(t).or_insert(0) += 1;
        first_seen.entry(t).or_insert(i);
        total += 1;
    }
    println!("Scanned {} ({} NAL units)", path, total);
    for (t, n) in &counts {
        println!("  type {:>2} ({:<32}) x {:>6}  first at NAL #{}", t, type_name(*t), n, first_seen[t]);
    }
    Ok(())
}
