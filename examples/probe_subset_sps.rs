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
// The Subset SPS RBSP is decoded in full: the base
// seq_parameter_set_data() (geometry, POC, VUI/HRD) followed by the MVC
// extension (view count, inter-view reference lists, level/operating
// points). This is the real-bitstream check that src/mvc/sps.rs +
// nal.rs + rbsp.rs agree with what a 3D Blu-ray actually emits.

use std::fs;

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::nal::{parse_nal_unit_header, NAL_SUBSET_SPS};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::sps::parse_subset_sps_rbsp;

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
        match parse_subset_sps_rbsp(&rbsp) {
            Ok(subset) => {
                let s = &subset.sps;
                eprintln!("  level_idc          = {}", s.level_idc);
                eprintln!(
                    "  dimensions         = {}x{} (chroma_format_idc {})",
                    s.width, s.height, s.chroma_format_idc
                );
                eprintln!("  max_num_ref_frames = {}", s.max_num_ref_frames);
                if let Some(t) = &s.vui_timing {
                    eprintln!(
                        "  vui timing         = {}/{} ({:.3} fps)",
                        t.num_units_in_tick,
                        t.time_scale,
                        t.time_scale as f64 / (2.0 * t.num_units_in_tick as f64)
                    );
                }
                eprintln!("  views              = {}", subset.mvc.num_views_minus1 + 1);
                eprintln!("  view_ids           = {:?}", subset.mvc.view_id);
                eprintln!("  anchor refs (v1)   = l0:{:?} l1:{:?}",
                    subset.mvc.anchor_refs.get(1).map(|r| &r.l0),
                    subset.mvc.anchor_refs.get(1).map(|r| &r.l1));
            }
            Err(e) => eprintln!("  subset SPS decode FAILED: {e:?}"),
        }
        if count >= 5 {
            break;
        }
    }
    eprintln!("scanned {count} Subset SPS NAL(s)");
    Ok(())
}
