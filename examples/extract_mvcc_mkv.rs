// Run mvcC BlockAddition extractor against an MKV and report what came
// out. Usage:
//
//   cargo run --example extract_mvcc_mkv --release -- in.mkv out.h264

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::ebml::EbmlReader;
use ripsaw::mvc::mkv_extract::extract_to_annex_b;
use ripsaw::mvc::nal::parse_nal_unit_header;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: extract_mvcc_mkv <in.mkv> <out.h264>"))?;
    let output = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing output path"))?;

    let in_file = File::open(PathBuf::from(&input))?;
    let out_file = File::create(PathBuf::from(&output))?;
    let mut reader = EbmlReader::new(in_file);
    let mut writer = BufWriter::new(out_file);

    let stats = extract_to_annex_b(&mut reader, &mut writer)?;
    drop(writer);
    println!(
        "extracted: frames={} base_nals={} dep_nals={}",
        stats.frames, stats.base_nals, stats.dep_nals,
    );

    // Sanity scan the produced file with our own NAL splitter and report
    // NAL-type histogram so we know we ended up with both views.
    let data = std::fs::read(&output)?;
    let mut counts: BTreeMap<u8, u64> = BTreeMap::new();
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        if let Ok((header, _)) = parse_nal_unit_header(nal) {
            *counts.entry(header.nal_unit_type).or_insert(0) += 1;
        }
    }
    println!("output NAL types:");
    for (t, n) in &counts {
        println!("  type {:>2} x {:>6}", t, n);
    }
    Ok(())
}
