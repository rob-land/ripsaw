// Run the Subset SPS parser against the first few type-15 NAL units in
// the input H.264 elementary stream, and report what was parseable.
// Used as a real-world sanity check that `src/mvc/sps.rs` +
// `src/mvc/nal.rs` + `src/mvc/rbsp.rs` agree with what's on the wire.
//
// Usage:
//   cargo run --release --example probe_subset_sps -- <input.h264>
//
// To get a usable input:
//   cargo run --release --example extract_mvcc_mkv -- \
//     samples/3D_LR_Pattern.mkv /tmp/lrpat.h264
//   cargo run --release --example probe_subset_sps -- /tmp/lrpat.h264
//
// Note on coverage: the MVC extension within a Subset SPS sits AFTER
// the base seq_parameter_set_data() in the same RBSP. We don't yet
// have a base-SPS parser (it'd be a ~200-line port of H.264 § 7.3.2.1);
// until that exists this example can only verify NAL discovery + RBSP
// extraction + the first byte (profile_idc), not the MVC extension
// payload itself. That's the next concrete libmvc TODO -- see
// `docs/libmvc.md` § "What to do *now*".

use std::fs;

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::nal::{parse_nal_unit_header, NAL_SUBSET_SPS};
use ripsaw::mvc::rbsp::extract_rbsp;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: probe_subset_sps <input.h264>"))?;
    let data = fs::read(&path)?;
    eprintln!("read {} ({} bytes)", path, data.len());

    let mut count = 0;
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let (header, consumed) = match parse_nal_unit_header(nal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if header.nal_unit_type != NAL_SUBSET_SPS {
            continue;
        }
        count += 1;
        let rbsp = extract_rbsp(&nal[consumed..]);
        eprintln!("--- Subset SPS #{count} ---");
        eprintln!("  forbidden_zero_bit = {}", header.forbidden_zero_bit);
        eprintln!("  nal_ref_idc        = {}", header.nal_ref_idc);
        eprintln!("  nal_unit_type      = {} (Subset SPS)", header.nal_unit_type);
        eprintln!("  RBSP byte length   = {}", rbsp.len());
        eprintln!("  RBSP first bytes   = {:02X?}", &rbsp[..rbsp.len().min(16)]);
        let profile_idc = rbsp.first().copied().unwrap_or(0);
        let profile_name = match profile_idc {
            118 => "Multiview High",
            128 => "Stereo High",
            134 => "MFC High",
            other => {
                eprintln!(
                    "  profile_idc        = {other} (NOT in the Annex G family)"
                );
                continue;
            }
        };
        eprintln!("  profile_idc        = {profile_idc} ({profile_name})");
        if rbsp.len() >= 4 {
            eprintln!(
                "  constraint flags   = 0x{:02X}",
                rbsp[1]
            );
            eprintln!("  level_idc          = {} (encoded as 0x{:02X})", rbsp[3], rbsp[3]);
        }
        if count >= 5 {
            break;
        }
    }
    eprintln!("scanned {count} Subset SPS NAL(s)");
    Ok(())
}
