// Raw Byte Sequence Payload extraction. H.264 wraps RBSP in a NAL unit's
// EBSP (Encapsulated Byte Sequence Payload) by inserting a 0x03 byte
// after any 00 00 sequence (the "emulation prevention three byte"). We
// need the clean RBSP for the bit reader to parse.
//
// See H.264 § 7.4.1.1 "Encapsulation of an SODB within an RBSP".

/// Strip emulation-prevention bytes from a NAL unit's EBSP payload to
/// recover the underlying RBSP. The first NAL header byte must NOT be
/// included in `ebsp` -- pass only the payload after `nal_unit_header()`.
pub fn extract_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zero_run = 0u8;
    for &b in ebsp {
        if zero_run >= 2 && b == 0x03 {
            // Skip the emulation prevention byte; reset run-of-zeros
            // counter because the spec allows another 0x03 only after
            // at least two more 00 bytes.
            zero_run = 0;
            continue;
        }
        out.push(b);
        if b == 0x00 {
            zero_run += 1;
        } else {
            zero_run = 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_payload_with_no_escape_sequences() {
        let input = vec![0x42, 0x00, 0x01, 0x80];
        assert_eq!(extract_rbsp(&input), input);
    }

    #[test]
    fn strips_emulation_prevention_byte_after_two_zeros() {
        // 00 00 03 04 -> 00 00 04 (drop the 03)
        let input = vec![0x00, 0x00, 0x03, 0x04];
        assert_eq!(extract_rbsp(&input), vec![0x00, 0x00, 0x04]);
    }

    #[test]
    fn preserves_03_when_not_preceded_by_two_zeros() {
        // 01 03 04 -> 01 03 04 (keep the 03; it isn't an escape)
        let input = vec![0x01, 0x03, 0x04];
        assert_eq!(extract_rbsp(&input), input);
    }

    #[test]
    fn handles_multiple_escape_sequences_in_one_payload() {
        // 00 00 03 00 00 03 -> 00 00 00 00
        let input = vec![0x00, 0x00, 0x03, 0x00, 0x00, 0x03];
        assert_eq!(extract_rbsp(&input), vec![0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn handles_run_of_three_zeros_followed_by_escape() {
        // 00 00 00 03 -> 00 00 00 (after seeing two zeros the parser
        // recognises the next 03 as an escape and drops it; the third
        // zero is between two valid bytes, so it stays).
        let input = vec![0x00, 0x00, 0x00, 0x03];
        assert_eq!(extract_rbsp(&input), vec![0x00, 0x00, 0x00]);
    }

    #[test]
    fn handles_lone_03_after_a_single_zero() {
        // 00 03 04 -> 00 03 04 (only one leading zero so 03 stays).
        let input = vec![0x00, 0x03, 0x04];
        assert_eq!(extract_rbsp(&input), input);
    }

    #[test]
    fn empty_input_yields_empty_rbsp() {
        assert!(extract_rbsp(&[]).is_empty());
    }
}
