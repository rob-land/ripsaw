// NAL unit header parsing, including the MVC extension that Annex G
// adds for NAL unit types 14, 20, and 21. See H.264 § 7.3.1 and
// § G.7.3.1.
//
// Layout (after the 0x000001 byte-stream start code, which the demuxer
// has already stripped):
//
//   forbidden_zero_bit   u(1)
//   nal_ref_idc          u(2)
//   nal_unit_type        u(5)
//   if nal_unit_type in {14, 20, 21}:
//       if nal_unit_type != 21:
//           svc_extension_flag  u(1)
//           if svc_extension_flag == 1:
//               nal_unit_header_svc_extension()   // 24 bits -- not parsed here
//           else:
//               nal_unit_header_mvc_extension()   // 24 bits -- parsed below

use super::bitstream::{BitReader, ReadError};

pub const NAL_PREFIX: u8 = 14;
pub const NAL_SLICE_LAYER_EXTENSION: u8 = 20;
pub const NAL_VIEW_COMPONENT_SCALABLE: u8 = 21;
pub const NAL_SPS: u8 = 7;
pub const NAL_SUBSET_SPS: u8 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnitHeader {
    pub forbidden_zero_bit: bool,
    pub nal_ref_idc: u8,
    pub nal_unit_type: u8,
    /// Present only for NAL types 14, 20, 21 with svc_extension_flag == 0.
    pub mvc_extension: Option<MvcNalExtension>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvcNalExtension {
    pub non_idr_flag: bool,
    pub priority_id: u8,
    pub view_id: u16,
    pub temporal_id: u8,
    pub anchor_pic_flag: bool,
    pub inter_view_flag: bool,
    pub reserved_one_bit: bool,
}

/// Parse a NAL unit header (1 byte + optional 3-byte MVC extension) from
/// the start of the NAL unit's bytes. Returns the parsed header and the
/// number of bytes consumed (1 or 4). The caller can then slice
/// `nal_bytes[consumed..]` and run it through `rbsp::extract_rbsp` to
/// get the bit-readable payload.
pub fn parse_nal_unit_header(nal_bytes: &[u8]) -> Result<(NalUnitHeader, usize), ReadError> {
    if nal_bytes.is_empty() {
        return Err(ReadError::Truncated);
    }
    let first = nal_bytes[0];
    let forbidden_zero_bit = (first >> 7) & 1 == 1;
    let nal_ref_idc = (first >> 5) & 0b11;
    let nal_unit_type = first & 0b0001_1111;

    let needs_extension = matches!(
        nal_unit_type,
        NAL_PREFIX | NAL_SLICE_LAYER_EXTENSION | NAL_VIEW_COMPONENT_SCALABLE
    ) && nal_unit_type != NAL_VIEW_COMPONENT_SCALABLE;
    // ^ G.7.3.1: type 21 doesn't carry the extension flag in the
    //   Annex-G branch; the extension subroutines below are gated by it
    //   for types 14 and 20. We follow the spec literally.

    if !needs_extension {
        return Ok((
            NalUnitHeader {
                forbidden_zero_bit,
                nal_ref_idc,
                nal_unit_type,
                mvc_extension: None,
            },
            1,
        ));
    }

    if nal_bytes.len() < 4 {
        return Err(ReadError::Truncated);
    }

    let mut reader = BitReader::new(&nal_bytes[1..4]);
    let svc_extension_flag = reader.read_bit()?;
    if svc_extension_flag {
        // SVC extension; we don't model it.
        return Ok((
            NalUnitHeader {
                forbidden_zero_bit,
                nal_ref_idc,
                nal_unit_type,
                mvc_extension: None,
            },
            4,
        ));
    }

    let non_idr_flag = reader.read_bit()?;
    let priority_id = reader.read_u(6)? as u8;
    let view_id = reader.read_u(10)? as u16;
    let temporal_id = reader.read_u(3)? as u8;
    let anchor_pic_flag = reader.read_bit()?;
    let inter_view_flag = reader.read_bit()?;
    let reserved_one_bit = reader.read_bit()?;

    Ok((
        NalUnitHeader {
            forbidden_zero_bit,
            nal_ref_idc,
            nal_unit_type,
            mvc_extension: Some(MvcNalExtension {
                non_idr_flag,
                priority_id,
                view_id,
                temporal_id,
                anchor_pic_flag,
                inter_view_flag,
                reserved_one_bit,
            }),
        },
        4,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_sps_nal_with_no_extension() {
        // Type 7 (SPS), nal_ref_idc 3, forbidden_zero_bit 0.
        // first byte = 0b0_11_00111 = 0x67
        let header = parse_nal_unit_header(&[0x67]).unwrap();
        assert_eq!(header.1, 1);
        assert_eq!(header.0.nal_unit_type, 7);
        assert_eq!(header.0.nal_ref_idc, 3);
        assert!(!header.0.forbidden_zero_bit);
        assert!(header.0.mvc_extension.is_none());
    }

    #[test]
    fn parses_subset_sps_with_no_extension_bytes() {
        // Type 15 (Subset SPS), nal_ref_idc 3.
        // first byte = 0b0_11_01111 = 0x6F. Subset SPS uses the base
        // NAL header only (no MVC extension byte block); the MVC SPS
        // body itself lives in the RBSP that follows.
        let (header, consumed) = parse_nal_unit_header(&[0x6F]).unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(header.nal_unit_type, 15);
        assert!(header.mvc_extension.is_none());
    }

    #[test]
    fn parses_mvc_slice_layer_extension_nal() {
        // Type 20 (slice_layer_extension), nal_ref_idc 3 -> first byte 0x74.
        // Extension byte block (3 bytes):
        //   bit  0:    svc_extension_flag = 0  (MVC branch)
        //   bit  1:    non_idr_flag       = 1
        //   bits 2-7:  priority_id        = 0
        //   bits 8-17: view_id            = 1
        //   bits 18-20: temporal_id       = 2
        //   bit  21:   anchor_pic_flag    = 0
        //   bit  22:   inter_view_flag    = 1
        //   bit  23:   reserved_one_bit   = 1
        //
        // Pack:
        //   0 1 000000 0000000001 010 0 1 1
        // = 0100 0000 0000 0000 0010 1001 1
        // Wait, that's 25 bits. Let me redo as 24 bits in the right
        // order with one bit per the layout above:
        //   bit0=0 bit1=1 bits2..7=000000 bits8..17=0000000001
        //   bits18..20=010 bit21=0 bit22=1 bit23=1
        // = 0 1 000000 0000000001 010 0 1 1
        // = 0100 0000 0000 0000 0010 1001 1   (oops 25 bits)
        // Recount: 1 + 1 + 6 + 10 + 3 + 1 + 1 + 1 = 24. Good.
        //   0 | 1 | 000000 | 0000000001 | 010 | 0 | 1 | 1
        //   = "0100000000000000010100" hmm that's 22. Let me list bit-
        //   by-bit:
        //   0 1 0 0 0 0 0 0  -> 0x40
        //   0 0 0 0 0 0 0 0  -> 0x00 (the 10-bit view_id has 8 zeros then a 1)
        //   0 1 0 1 0 0 1 1  -> 0x53
        // So bytes are [0x40, 0x00, 0x53] -- but wait, the second byte
        // gets view_id bits 0..7 of the 10-bit field i.e. the high bits.
        // view_id = 1 = 0b0000000001 (MSB first). High 8 bits = 0x00.
        // Continue: low 2 bits of view_id + temporal_id(3) + flag(1)+(1)+(1)
        //   = 01 010 0 1 1 = 0b01010011 = 0x53
        // So ext = [0x40, 0x00, 0x53].
        let buf = [0x74, 0x40, 0x00, 0x53];
        let (header, consumed) = parse_nal_unit_header(&buf).unwrap();
        assert_eq!(consumed, 4);
        assert_eq!(header.nal_unit_type, 20);
        let ext = header.mvc_extension.expect("extension present");
        assert!(ext.non_idr_flag);
        assert_eq!(ext.priority_id, 0);
        assert_eq!(ext.view_id, 1);
        assert_eq!(ext.temporal_id, 2);
        assert!(!ext.anchor_pic_flag);
        assert!(ext.inter_view_flag);
        assert!(ext.reserved_one_bit);
    }

    #[test]
    fn truncated_buffer_yields_error() {
        assert!(parse_nal_unit_header(&[]).is_err());
        // Type 20 needs 4 bytes total
        assert!(parse_nal_unit_header(&[0x74]).is_err());
        assert!(parse_nal_unit_header(&[0x74, 0x40]).is_err());
    }

    #[test]
    fn svc_extension_flag_short_circuits_mvc_parse() {
        // Type 14 (prefix NAL) with svc_extension_flag = 1 -> we don't
        // populate mvc_extension, but we still consume 4 bytes.
        // First byte = 0b0_11_01110 = 0x6E
        // Ext byte 1 high bit (svc flag) = 1 -> 0x80
        let buf = [0x6E, 0x80, 0x00, 0x00];
        let (header, consumed) = parse_nal_unit_header(&buf).unwrap();
        assert_eq!(consumed, 4);
        assert_eq!(header.nal_unit_type, 14);
        assert!(header.mvc_extension.is_none());
    }
}
