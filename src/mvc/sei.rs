// SEI (Supplemental Enhancement Information) messages relevant to MVC.
// Right now we implement the one critical to stereo output:
//
//   frame_packing_arrangement (payload type 45)  --  D.1.26 / D.2.26
//
// This SEI tells downstream players how the two views are packed
// (side-by-side / top-bottom / frame-sequential / ...). FSBS output
// from our converter needs to emit a frame_packing_arrangement_type=3
// SEI so Jellyfin/Plex/VLC pick up the stereoscopic nature of the
// file.
//
// The SEI itself sits inside a NAL unit of type 6 (SEI), preceded by
// the variable-length payload_type and payload_size fields per
// H.264 § 7.3.2.3.

use super::bitstream::{BitReader, ReadError};

pub const SEI_PAYLOAD_TYPE_FRAME_PACKING_ARRANGEMENT: u32 = 45;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePackingArrangement {
    pub id: u32,
    pub cancel_flag: bool,
    pub body: Option<FramePackingArrangementBody>,
    pub extension_flag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePackingArrangementBody {
    pub arrangement_type: u8,
    pub quincunx_sampling_flag: bool,
    pub content_interpretation_type: u8,
    pub spatial_flipping_flag: bool,
    pub frame0_flipped_flag: bool,
    pub field_views_flag: bool,
    pub current_frame_is_frame0_flag: bool,
    pub frame0_self_contained_flag: bool,
    pub frame1_self_contained_flag: bool,
    pub grid_positions: Option<GridPositions>,
    pub reserved_byte: u8,
    pub repetition_period: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridPositions {
    pub frame0_x: u8,
    pub frame0_y: u8,
    pub frame1_x: u8,
    pub frame1_y: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrangementType {
    Checkerboard = 0,
    ColumnInterleaved = 1,
    RowInterleaved = 2,
    SideBySide = 3,
    TopBottom = 4,
    TemporalInterleaving = 5,
    TileFormat = 6,
}

impl ArrangementType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ArrangementType::Checkerboard),
            1 => Some(ArrangementType::ColumnInterleaved),
            2 => Some(ArrangementType::RowInterleaved),
            3 => Some(ArrangementType::SideBySide),
            4 => Some(ArrangementType::TopBottom),
            5 => Some(ArrangementType::TemporalInterleaving),
            6 => Some(ArrangementType::TileFormat),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

pub fn parse_frame_packing_arrangement(
    reader: &mut BitReader<'_>,
) -> Result<FramePackingArrangement, ReadError> {
    let id = reader.read_ue()?;
    let cancel_flag = reader.read_bit()?;
    let body = if !cancel_flag {
        let arrangement_type = reader.read_u(7)? as u8;
        let quincunx_sampling_flag = reader.read_bit()?;
        let content_interpretation_type = reader.read_u(6)? as u8;
        let spatial_flipping_flag = reader.read_bit()?;
        let frame0_flipped_flag = reader.read_bit()?;
        let field_views_flag = reader.read_bit()?;
        let current_frame_is_frame0_flag = reader.read_bit()?;
        let frame0_self_contained_flag = reader.read_bit()?;
        let frame1_self_contained_flag = reader.read_bit()?;

        let grid_positions = if !quincunx_sampling_flag && arrangement_type != 5 {
            Some(GridPositions {
                frame0_x: reader.read_u(4)? as u8,
                frame0_y: reader.read_u(4)? as u8,
                frame1_x: reader.read_u(4)? as u8,
                frame1_y: reader.read_u(4)? as u8,
            })
        } else {
            None
        };

        let reserved_byte = reader.read_u(8)? as u8;
        let repetition_period = reader.read_ue()?;
        Some(FramePackingArrangementBody {
            arrangement_type,
            quincunx_sampling_flag,
            content_interpretation_type,
            spatial_flipping_flag,
            frame0_flipped_flag,
            field_views_flag,
            current_frame_is_frame0_flag,
            frame0_self_contained_flag,
            frame1_self_contained_flag,
            grid_positions,
            reserved_byte,
            repetition_period,
        })
    } else {
        None
    };
    let extension_flag = reader.read_bit()?;
    Ok(FramePackingArrangement { id, cancel_flag, body, extension_flag })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_to_bytes(s: &str) -> Vec<u8> {
        let mut s = s.replace(|c: char| c == ' ' || c == '|', "");
        let pad = (8 - (s.len() % 8)) % 8;
        s.extend(std::iter::repeat('0').take(pad));
        s.as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect()
    }

    #[test]
    fn arrangement_type_round_trip() {
        for v in 0u8..=6 {
            let t = ArrangementType::from_u8(v).expect("valid type");
            assert_eq!(t.as_u8(), v);
        }
        assert_eq!(ArrangementType::from_u8(7), None);
    }

    #[test]
    fn parses_canonical_side_by_side_fpa() {
        // id=0 (ue "1"); cancel=0; type=3 (u(7) 0000011); quincunx=0;
        // content_interp=1 (u(6) 000001); spatial_flip=0; frame0_flip=0;
        // field_views=0; current_is_frame0=1;
        // frame0_self_contained=1; frame1_self_contained=1;
        // grid present: f0x=0, f0y=8, f1x=8, f1y=8 (each u(4))
        // reserved=0 (u(8)); repetition=0 (ue "1"); ext_flag=0
        let bits = [
            "1",            // id = 0
            "0",            // cancel = 0
            "0000011",      // type = 3
            "0",            // quincunx = 0
            "000001",       // content_interp = 1
            "0",            // spatial_flip
            "0",            // frame0_flip
            "0",            // field_views
            "1",            // current_is_frame0
            "1",            // frame0_self_contained
            "1",            // frame1_self_contained
            "0000",         // f0x = 0
            "1000",         // f0y = 8
            "1000",         // f1x = 8
            "1000",         // f1y = 8
            "00000000",     // reserved byte
            "1",            // repetition = 0 (ue)
            "0",            // extension_flag
        ];
        let bytes = bits_to_bytes(&bits.concat());
        let mut r = BitReader::new(&bytes);
        let fpa = parse_frame_packing_arrangement(&mut r).expect("parse");
        assert_eq!(fpa.id, 0);
        assert!(!fpa.cancel_flag);
        let body = fpa.body.expect("body present");
        assert_eq!(body.arrangement_type, 3);
        assert!(!body.quincunx_sampling_flag);
        assert_eq!(body.content_interpretation_type, 1);
        assert!(body.current_frame_is_frame0_flag);
        let grid = body.grid_positions.expect("grid present");
        assert_eq!((grid.frame0_x, grid.frame0_y), (0, 8));
        assert_eq!((grid.frame1_x, grid.frame1_y), (8, 8));
        assert_eq!(body.repetition_period, 0);
        assert!(!fpa.extension_flag);
    }

    #[test]
    fn cancel_flag_skips_body() {
        // id = 0 (ue "1"); cancel = 1; extension_flag = 0
        let bytes = bits_to_bytes("1 1 0");
        let mut r = BitReader::new(&bytes);
        let fpa = parse_frame_packing_arrangement(&mut r).expect("parse");
        assert_eq!(fpa.id, 0);
        assert!(fpa.cancel_flag);
        assert!(fpa.body.is_none());
        assert!(!fpa.extension_flag);
    }

    #[test]
    fn temporal_interleaving_skips_grid_positions() {
        // type = 5 -> grid_positions absent regardless of quincunx
        // id=0 cancel=0 type=5 (0000101) quincunx=0 content_interp=0
        // (000000) spatial=0 f0flip=0 fview=0 cur_is_f0=1
        // f0self=1 f1self=1 reserved=0 repetition=0 (ue "1") ext=0
        let bits = [
            "1",        // id
            "0",        // cancel
            "0000101",  // type 5
            "0",        // quincunx
            "000000",   // content_interp
            "0",        // spatial
            "0",        // f0flip
            "0",        // fview
            "1",        // cur_is_f0
            "1",        // f0self
            "1",        // f1self
            // (NO grid positions because type == 5)
            "00000000", // reserved
            "1",        // repetition (ue 0)
            "0",        // ext
        ];
        let bytes = bits_to_bytes(&bits.concat());
        let mut r = BitReader::new(&bytes);
        let fpa = parse_frame_packing_arrangement(&mut r).expect("parse");
        let body = fpa.body.expect("body");
        assert_eq!(body.arrangement_type, 5);
        assert!(body.grid_positions.is_none());
    }
}
