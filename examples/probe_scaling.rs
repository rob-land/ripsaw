// Probe the scaling matrices captured from a stream's SPS/PPS and print the
// resolved 8×8 intra-luma matrix (to validate against JM's qmatrix dump).
//   cargo run --release --example probe_scaling -- base1.h264

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::parse_pic_parameter_set;
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: probe_scaling <stream.h264>"))?;
    let data = std::fs::read(&path)?;
    let mut sps: Option<Sps> = None;
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((h, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match h.nal_unit_type {
            7 => {
                let s = parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?;
                println!("SPS scaling present: {}", s.scaling.is_some());
                if let Some(sc) = &s.scaling {
                    print_8x8("SPS list 6 (8x8 intra Y)", sc.intra_8x8_luma());
                }
                sps = Some(s);
            }
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                println!("PPS scaling present: {}", p.scaling.is_some());
                if let Some(sc) = &p.scaling {
                    print_8x8("PPS list 6 (8x8 intra Y)", sc.intra_8x8_luma());
                    // JM's dumped qmatrix[6] for this stream, for comparison.
                    let jm: [[i32; 8]; 8] = [
                        [6, 13, 15, 16, 17, 19, 20, 21],
                        [13, 14, 15, 16, 18, 19, 20, 21],
                        [15, 15, 16, 17, 18, 19, 20, 21],
                        [16, 16, 17, 18, 19, 20, 21, 22],
                        [17, 18, 18, 19, 19, 20, 21, 22],
                        [19, 19, 19, 20, 20, 21, 22, 23],
                        [20, 20, 20, 21, 21, 22, 22, 23],
                        [21, 21, 21, 22, 22, 23, 23, 24],
                    ];
                    println!("MATCHES JM qmatrix[6]: {}", sc.intra_8x8_luma() == &jm);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn print_8x8(label: &str, m: &[[i32; 8]; 8]) {
    println!("{label}:");
    for row in m {
        println!("  {row:?}");
    }
}
