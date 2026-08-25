// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal base64 decoder for `!!binary` scalars.
//!
//! Hand-written rather than taken as a dependency. This crate's entire
//! dependency set is `glaucus-ast`, `glaucus-core` and `serde`; `!!binary` needs
//! a lookup table and one accumulator loop, and pulling in a crate — plus its
//! advisory surface and MSRV — to avoid fifty lines would be the larger cost.
//!
//! Decoding is deliberately **strict**. A `!!binary` scalar that does not decode
//! is a malformed document, and silently handing back the undecoded text would
//! turn a document defect into a wrong value in the caller's data.

/// Not part of the base64 alphabet.
const INVALID: u8 = 0xFF;
/// The `=` padding character.
const PAD: u8 = 0xFE;
/// ASCII whitespace, which YAML permits inside a `!!binary` payload.
const SKIP: u8 = 0xFD;

/// Reverse alphabet: byte value → 6-bit sextet, or one of the sentinels above.
static DECODE: [u8; 256] = build_table();

const fn build_table() -> [u8; 256] {
    let mut table = [INVALID; 256];

    let mut i = 0u8;
    while i < 26 {
        table[(b'A' + i) as usize] = i;
        table[(b'a' + i) as usize] = 26 + i;
        i += 1;
    }

    let mut d = 0u8;
    while d < 10 {
        table[(b'0' + d) as usize] = 52 + d;
        d += 1;
    }

    table[b'+' as usize] = 62;
    table[b'/' as usize] = 63;
    table[b'=' as usize] = PAD;

    // YAML 1.2 §10.3.2 presents `!!binary` payloads as block scalars, so line
    // breaks and indentation are expected inside the data and are not part of it.
    table[b' ' as usize] = SKIP;
    table[b'\t' as usize] = SKIP;
    table[b'\n' as usize] = SKIP;
    table[b'\r' as usize] = SKIP;

    table
}

/// The standard base64 alphabet, indexed by sextet.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `bytes` as standard-alphabet base64, **padded**.
///
/// Padding is emitted rather than truncated. [`decode`] accepts either, but a
/// `!!binary` scalar is a value that leaves this process, and another YAML reader
/// may not be as lenient — so the canonical form is what goes out.
pub(crate) fn encode(bytes: &[u8]) -> String {
    // Four output characters per three input bytes, rounded up.
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        // Pack the chunk into the low 24 bits, zero-filling a short tail.
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let packed = (b0 << 16) | (b1 << 8) | b2;

        for i in 0..4 {
            if i <= chunk.len() {
                let sextet = (packed >> (18 - 6 * i)) & 0x3F;
                out.push(ALPHABET[sextet as usize] as char);
            } else {
                out.push('=');
            }
        }
    }

    out
}

/// Decodes a standard-alphabet base64 payload.
///
/// Returns `None` for any input that is not valid base64: a character outside
/// the alphabet, data following padding, a dangling sextet that cannot complete
/// a byte, or non-zero bits in the final partial group.
pub(crate) fn decode(input: &str) -> Option<Vec<u8>> {
    // Three output bytes per four input characters; over-allocates slightly when
    // the payload carries whitespace, which is cheaper than growing.
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u8 = 0;
    let mut seen_padding = false;

    for &byte in input.as_bytes() {
        match DECODE[byte as usize] {
            SKIP => {}
            PAD => seen_padding = true,
            INVALID => return None,
            sextet => {
                // Data after `=` means the padding was not terminal, so the
                // payload is malformed rather than merely oddly padded.
                if seen_padding {
                    return None;
                }
                acc = (acc << 6) | u32::from(sextet);
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    // Truncation is the intent: `bits` is at most 6 here, so the
                    // shift leaves exactly the byte being emitted.
                    #[allow(clippy::cast_possible_truncation)]
                    out.push((acc >> bits) as u8);
                }
            }
        }
    }

    // A single leftover sextet encodes no whole byte, so the input was truncated.
    if bits >= 6 {
        return None;
    }
    // The bits that remain are padding and must be zero; anything else means the
    // encoder and this decoder disagree about the payload.
    if bits > 0 && acc & ((1u32 << bits) - 1) != 0 {
        return None;
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn table_maps_the_alphabet_and_sentinels() {
        // `DECODE` is a `static` initialised by a `const fn`, so the builder runs
        // at COMPILE time and no runtime instrumentation ever observes it. Calling
        // it here executes the same code at runtime, which both covers it and
        // checks the table it produces rather than trusting the loops by eye.
        let table = super::build_table();

        for (i, c) in (b'A'..=b'Z').enumerate() {
            assert_eq!(table[c as usize], u8::try_from(i).unwrap(), "{}", c as char);
        }
        for (i, c) in (b'a'..=b'z').enumerate() {
            assert_eq!(table[c as usize], 26 + u8::try_from(i).unwrap());
        }
        for (i, c) in (b'0'..=b'9').enumerate() {
            assert_eq!(table[c as usize], 52 + u8::try_from(i).unwrap());
        }
        assert_eq!(table[b'+' as usize], 62);
        assert_eq!(table[b'/' as usize], 63);
        assert_eq!(table[b'=' as usize], super::PAD);
        for ws in *b" \t\n\r" {
            assert_eq!(table[ws as usize], super::SKIP);
        }
        // Everything outside the alphabet must be rejected, not silently mapped.
        for c in [b'*', b'@', b'-', b'_', 0u8, 0x7F, 0xFF] {
            assert_eq!(table[c as usize], super::INVALID, "byte {c:#04x}");
        }
    }

    #[test]
    fn encodes_the_canonical_example() {
        assert_eq!(super::encode(b"Hello World!"), "SGVsbG8gV29ybGQh");
    }

    #[test]
    fn encode_pads_rather_than_truncating() {
        assert_eq!(super::encode(b""), "");
        assert_eq!(super::encode(b"a"), "YQ==");
        assert_eq!(super::encode(b"ab"), "YWI=");
        assert_eq!(super::encode(b"abc"), "YWJj");
    }

    /// Round-trips at every padding length. 0-7 bytes covers each remainder of
    /// three at least twice, so a mis-shifted tail cannot hide.
    #[test]
    fn encode_decode_round_trips_at_every_padding_length() {
        for len in 0..=7usize {
            let data: Vec<u8> = (0..len)
                .map(|i| u8::try_from(i * 37 % 256).unwrap())
                .collect();
            let encoded = super::encode(&data);
            assert_eq!(
                decode(&encoded).as_deref(),
                Some(data.as_slice()),
                "len {len} did not round-trip (encoded: {encoded})"
            );
            assert_eq!(encoded.len() % 4, 0, "len {len} produced unpadded output");
        }
    }

    #[test]
    fn encode_round_trips_all_byte_values() {
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(
            decode(&super::encode(&data)).as_deref(),
            Some(data.as_slice())
        );
    }

    #[test]
    fn decodes_the_canonical_example() {
        assert_eq!(decode("SGVsbG8gV29ybGQh").unwrap(), b"Hello World!");
    }

    #[test]
    fn decodes_every_padding_length() {
        assert_eq!(decode("YQ==").unwrap(), b"a");
        assert_eq!(decode("YWI=").unwrap(), b"ab");
        assert_eq!(decode("YWJj").unwrap(), b"abc");
        assert_eq!(decode("").unwrap(), b"");
    }

    #[test]
    fn skips_whitespace_inside_the_payload() {
        // A block-scalar payload arrives with line breaks and indentation.
        assert_eq!(decode("SGVsbG8g\n  V29ybGQh").unwrap(), b"Hello World!");
        assert_eq!(decode("YQ ==").unwrap(), b"a");
    }

    #[test]
    fn rejects_characters_outside_the_alphabet() {
        assert!(decode("not@valid@base64").is_none());
        assert!(decode("SGVsbG8*").is_none());
        assert!(decode("YQ==!").is_none());
    }

    #[test]
    fn rejects_data_after_padding() {
        assert!(decode("YQ==YQ==").is_none());
    }

    #[test]
    fn rejects_a_truncated_final_group() {
        // One leftover sextet cannot complete a byte.
        assert!(decode("YWJjY").is_none());
    }

    #[test]
    fn rejects_non_zero_padding_bits() {
        // `YR` carries a non-zero remainder in bits that padding must leave clear.
        assert!(decode("YR").is_none());
    }

    #[test]
    fn round_trips_all_byte_values() {
        // Differential check against a scalar reference encoder, over every byte,
        // so the table cannot be subtly wrong for a rarely used character.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let data: Vec<u8> = (0..=255u8).collect();

        let mut encoded = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    encoded.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
                } else {
                    encoded.push('=');
                }
            }
        }

        assert_eq!(decode(&encoded).unwrap(), data);
    }
}
