//! Streaming, no-alloc base64 decoder (standard alphabet, `=` padding).
//!
//! Used by the desktop layer (Task 8) to decode `METADATA_BLOCK_PICTURE`
//! comment values pointed at by an Ogg [`crate::Picture`] descriptor whose
//! `encoding` is [`crate::DataEncoding::Base64`].

use crate::TagError;

/// Map one base64 alphabet character to its 6-bit value.
fn decode_char(c: u8) -> Result<u8, TagError> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(TagError::Malformed),
    }
}

/// Decode standard-alphabet, `=`-padded base64 `input` into `out`, returning
/// the number of bytes written.
///
/// `input`'s length must be a multiple of 4 (whole base64 groups); `=`
/// padding is only accepted in the last two positions of the final group.
/// Any other invalid character, or an `out` too small to hold the decoded
/// output, is rejected as [`TagError::Malformed`] — this never panics on
/// malformed or truncated input.
pub fn decode_into(input: &[u8], out: &mut [u8]) -> Result<usize, TagError> {
    if !input.len().is_multiple_of(4) {
        return Err(TagError::Malformed);
    }
    let n_chunks = input.len() / 4;
    let mut out_pos = 0usize;

    for (i, chunk) in input.chunks_exact(4).enumerate() {
        let is_last = i + 1 == n_chunks;
        let pad = match (chunk[2], chunk[3]) {
            (b'=', b'=') => 2,
            (c2, b'=') if c2 != b'=' => 1,
            (b'=', _) => return Err(TagError::Malformed), // '=' then non-'='
            _ => 0,
        };
        if pad > 0 && !is_last {
            return Err(TagError::Malformed); // padding mid-stream
        }
        if chunk[0] == b'=' || chunk[1] == b'=' {
            return Err(TagError::Malformed);
        }

        let b0 = decode_char(chunk[0])?;
        let b1 = decode_char(chunk[1])?;
        let b2 = if pad == 2 { 0 } else { decode_char(chunk[2])? };
        let b3 = if pad >= 1 { 0 } else { decode_char(chunk[3])? };

        let triple =
            (u32::from(b0) << 18) | (u32::from(b1) << 12) | (u32::from(b2) << 6) | u32::from(b3);
        let bytes = [(triple >> 16) as u8, (triple >> 8) as u8, triple as u8];

        let take = 3 - pad;
        let dst = out
            .get_mut(out_pos..out_pos + take)
            .ok_or(TagError::Malformed)?;
        dst.copy_from_slice(&bytes[..take]);
        out_pos += take;
    }

    Ok(out_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_with_one_padding_char() {
        let mut out = [0u8; 16];
        let n = decode_into(b"aGVsbG8=", &mut out).unwrap();
        assert_eq!(&out[..n], b"hello");
    }

    #[test]
    fn decodes_with_two_padding_chars() {
        let mut out = [0u8; 16];
        let n = decode_into(b"Zm8=", &mut out).unwrap();
        assert_eq!(&out[..n], b"fo");
    }

    #[test]
    fn decodes_exact_groups_without_padding() {
        let mut out = [0u8; 16];
        let n = decode_into(b"Zm9vYmFy", &mut out).unwrap();
        assert_eq!(&out[..n], b"foobar");
    }

    #[test]
    fn decodes_rfc4648_test_vectors() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"Zg==", b"f"),
            (b"Zm8=", b"fo"),
            (b"Zm9v", b"foo"),
            (b"Zm9vYg==", b"foob"),
            (b"Zm9vYmE=", b"fooba"),
            (b"Zm9vYmFy", b"foobar"),
        ];
        for (input, expected) in cases {
            let mut out = [0u8; 16];
            let n = decode_into(input, &mut out).unwrap();
            assert_eq!(&out[..n], *expected, "input {:?}", input);
        }
    }

    #[test]
    fn empty_input_decodes_to_empty() {
        let mut out = [0u8; 4];
        let n = decode_into(b"", &mut out).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rejects_invalid_character() {
        let mut out = [0u8; 8];
        assert_eq!(
            decode_into(b"aGVs*G8=", &mut out).unwrap_err(),
            TagError::Malformed
        );
    }

    #[test]
    fn rejects_length_not_multiple_of_four() {
        let mut out = [0u8; 8];
        assert_eq!(
            decode_into(b"abc", &mut out).unwrap_err(),
            TagError::Malformed
        );
    }

    #[test]
    fn rejects_padding_before_end_of_input() {
        let mut out = [0u8; 16];
        // Padding in the first group, more data after: not a valid tail.
        assert_eq!(
            decode_into(b"Zg==Zg==", &mut out).unwrap_err(),
            TagError::Malformed
        );
    }

    #[test]
    fn rejects_output_buffer_too_small() {
        let mut out = [0u8; 2];
        assert_eq!(
            decode_into(b"aGVsbG8=", &mut out).unwrap_err(),
            TagError::Malformed
        );
    }
}
