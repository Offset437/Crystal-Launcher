//! Crystal's bounded decoder for the official SLC and OLC patch formats.
//! The stream starts with a little-endian bit count. Tokens are LSB-first:
//! literal (0 + byte), output copy (1 + u16 distance + u8 count), and, in
//! OLC only, base copy (11 + i16 relative offset + u8 count).
//! Format evidence: installed POL core 73b1864b, 10042805/10042d91.

pub struct BitReader<'a> {
    input: &'a [u8],
    next_byte: usize,
    cache: u64,
    cached_bits: u32,
    bits_left: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(stream: &'a [u8]) -> Result<Self, String> {
        let (count, input) = stream
            .split_at_checked(4)
            .ok_or("coded stream shorter than its 4-byte header")?;
        let bits_left = u32::from_le_bytes(count.try_into().unwrap()) as usize;
        if bits_left.div_ceil(8) > input.len() {
            return Err(format!(
                "coded stream declares {bits_left} bits beyond its payload"
            ));
        }
        Ok(Self {
            input,
            next_byte: 0,
            cache: 0,
            cached_bits: 0,
            bits_left,
        })
    }

    pub fn remaining(&self) -> usize {
        self.bits_left
    }

    pub fn read(&mut self, width: u32) -> Result<u32, String> {
        if width > 32 || width as usize > self.bits_left {
            return Err(format!(
                "coded stream truncated or invalid field width {width}"
            ));
        }
        while self.cached_bits < width {
            self.cache |= u64::from(self.input[self.next_byte]) << self.cached_bits;
            self.next_byte += 1;
            self.cached_bits += 8;
        }
        let value = (self.cache & ((1u64 << width) - 1)) as u32;
        self.cache >>= width;
        self.cached_bits -= width;
        self.bits_left -= width as usize;
        Ok(value)
    }
}

enum Token {
    Literal(u8),
    History { distance: usize, count: usize },
    Base { displacement: i16, count: usize },
}

fn token(reader: &mut BitReader<'_>, delta: bool) -> Result<Token, String> {
    if reader.read(1)? == 0 {
        return Ok(Token::Literal(reader.read(8)? as u8));
    }
    let base = delta && reader.read(1)? != 0;
    let position = reader.read(16)? as u16;
    let count = reader.read(8)? as usize;
    Ok(if base {
        Token::Base {
            displacement: position as i16,
            count,
        }
    } else {
        Token::History {
            distance: position as usize,
            count,
        }
    })
}

fn expand(stream: &[u8], base: Option<&[u8]>, limit: usize) -> Result<Vec<u8>, String> {
    let mut bits = BitReader::new(stream)?;
    let mut output = Vec::with_capacity(
        base.map_or_else(|| stream.len().saturating_mul(2), <[u8]>::len)
            .min(limit),
    );
    while bits.remaining() != 0 {
        let item = token(&mut bits, base.is_some())?;
        let count = match item {
            Token::Literal(_) => 1,
            Token::History { count, .. } | Token::Base { count, .. } => count,
        };
        if count > limit.saturating_sub(output.len()) {
            return Err("patch output exceeds expected length".into());
        }
        match item {
            Token::Literal(byte) => output.push(byte),
            Token::History {
                distance,
                mut count,
            } => {
                let origin = output
                    .len()
                    .checked_sub(distance)
                    .filter(|_| distance != 0)
                    .ok_or_else(|| format!("invalid output-copy distance {distance}"))?;
                // Each extension grows the available prefix. This preserves
                // overlapping copies without one push per decoded byte.
                while count != 0 {
                    let available = output.len() - origin;
                    let take = count.min(available);
                    output.extend_from_within(origin..origin + take);
                    count -= take;
                }
            }
            Token::Base {
                displacement,
                count,
            } => {
                let reference = base.ok_or("base reference in direct patch")?;
                let origin = output
                    .len()
                    .checked_add_signed(displacement as isize)
                    .ok_or("base reference before beginning of file")?;
                let end = origin.checked_add(count).ok_or("base reference overflow")?;
                let bytes = reference
                    .get(origin..end)
                    .ok_or("base reference outside file")?;
                output.extend_from_slice(bytes);
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
pub fn decode_direct(stream: &[u8]) -> Result<Vec<u8>, String> {
    decode_direct_bounded(stream, 64 * 1024 * 1024)
}

pub fn decode_direct_bounded(stream: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    expand(stream, None, limit)
}

#[cfg(test)]
pub fn apply_indirect(stream: &[u8], base: &[u8]) -> Result<Vec<u8>, String> {
    apply_indirect_bounded(stream, base, 512 * 1024 * 1024)
}

pub fn apply_indirect_bounded(stream: &[u8], base: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    expand(stream, Some(base), limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Step {
        Byte(u8),
        Back(u16, u8),
        Source(i16, u8),
    }

    fn pack(fields: &[(u32, u32)]) -> Vec<u8> {
        let count = fields.iter().map(|field| field.1 as usize).sum::<usize>();
        let mut result = vec![0; 4 + count.div_ceil(8)];
        result[..4].copy_from_slice(&(count as u32).to_le_bytes());
        let mut cursor = 0;
        for &(value, width) in fields {
            for shift in 0..width {
                if value & (1 << shift) != 0 {
                    result[4 + cursor / 8] |= 1 << (cursor % 8);
                }
                cursor += 1;
            }
        }
        result
    }

    fn encode(steps: &[Step], delta: bool) -> Vec<u8> {
        let mut fields = Vec::new();
        for &step in steps {
            match step {
                Step::Byte(byte) => fields.extend([(0, 1), (byte.into(), 8)]),
                Step::Back(distance, count) => {
                    fields.push((1, if delta { 2 } else { 1 }));
                    fields.extend([(distance.into(), 16), (count.into(), 8)]);
                }
                Step::Source(displacement, count) => {
                    assert!(delta);
                    fields.extend([(3, 2), (displacement as u16 as u32, 16), (count.into(), 8)]);
                }
            }
        }
        pack(&fields)
    }

    // Abstract byte-by-byte model, independent of the decoder's buffered copies.
    fn expected(steps: &[Step], base: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        for &step in steps {
            match step {
                Step::Byte(byte) => output.push(byte),
                Step::Back(distance, count) => {
                    for _ in 0..count {
                        output.push(output[output.len() - distance as usize]);
                    }
                }
                Step::Source(displacement, count) => {
                    let start = (output.len() as isize + displacement as isize) as usize;
                    for &byte in &base[start..start + count as usize] {
                        output.push(byte);
                    }
                }
            }
        }
        output
    }

    fn noise(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| (index * 41 + index / 7 + 181) as u8)
            .collect()
    }

    #[test]
    fn reader_handles_zero_and_full_width_fields_at_every_alignment() {
        let fields: Vec<_> = (0..137u32)
            .map(|index| {
                let width = [1, 32, 4, 0, 31, 7, 9, 17, 25][index as usize % 9];
                let value = index.wrapping_mul(0x91d534a7).rotate_right(index % 32);
                (
                    if width == 0 {
                        0
                    } else {
                        value & (u32::MAX >> (32 - width))
                    },
                    width,
                )
            })
            .collect();
        let stream = pack(&fields);
        let mut reader = BitReader::new(&stream).unwrap();
        for &(value, width) in &fields {
            let available = reader.remaining();
            assert!(reader.read(33).is_err());
            assert_eq!(reader.remaining(), available);
            assert_eq!(reader.read(width).unwrap(), value);
        }
        assert_eq!(reader.remaining(), 0);
        assert_eq!(reader.read(0).unwrap(), 0);
        assert!(reader.read(1).is_err());
        assert!(BitReader::new(&u32::MAX.to_le_bytes()).is_err());
    }

    #[test]
    fn literals_cover_all_byte_values_in_direct_and_delta_streams() {
        for length in [0, 1, 2, 16, 257, 4096] {
            let data = noise(length);
            let steps: Vec<_> = data.iter().copied().map(Step::Byte).collect();
            assert_eq!(decode_direct(&encode(&steps, false)).unwrap(), data);
            assert_eq!(apply_indirect(&encode(&steps, true), &[]).unwrap(), data);
        }
    }

    #[test]
    fn overlapping_copies_match_a_byte_model_in_both_formats() {
        for distance in [1, 3, 17, 255] {
            for count in [0, 1, 2, 254, 255] {
                let mut steps: Vec<_> = noise(distance).into_iter().map(Step::Byte).collect();
                steps.push(Step::Back(distance as u16, count));
                let oracle = expected(&steps, &[]);
                assert_eq!(
                    decode_direct_bounded(&encode(&steps, false), oracle.len()).unwrap(),
                    oracle
                );
                assert_eq!(
                    apply_indirect_bounded(&encode(&steps, true), &[], oracle.len()).unwrap(),
                    oracle
                );
                assert!(decode_direct_bounded(&encode(&steps, false), oracle.len() - 1).is_err());
            }
        }
    }

    #[test]
    fn source_offsets_are_relative_to_output_and_support_signed_extremes() {
        let base = noise(40000);
        let cases = [
            vec![
                Step::Source(32767, 17),
                Step::Byte(0xa9),
                Step::Source(-13, 21),
                Step::Back(7, 29),
            ],
            {
                let mut steps: Vec<_> = noise(32768).into_iter().map(Step::Byte).collect();
                steps.push(Step::Source(i16::MIN, 31));
                steps
            },
        ];
        for steps in cases {
            let oracle = expected(&steps, &base);
            let stream = encode(&steps, true);
            assert_eq!(
                apply_indirect_bounded(&stream, &base, oracle.len()).unwrap(),
                oracle
            );
            assert!(apply_indirect_bounded(&stream, &base, oracle.len() - 1).is_err());
        }
    }

    #[test]
    fn generated_mixed_token_sequences_preserve_base_and_history_semantics() {
        let base = noise(8192);
        let mut seed = 0x392d6017u32;
        for round in 0..64 {
            let mut steps = vec![Step::Byte(round)];
            let mut length = 1;
            for _ in 0..71 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let count = (seed >> 16) as u8;
                let step = match seed % 3 {
                    0 => Step::Byte(seed as u8),
                    1 => Step::Back((seed as usize % length + 1) as u16, count),
                    _ => {
                        let origin = seed as usize % (base.len() - count as usize + 1);
                        Step::Source((origin as isize - length as isize) as i16, count)
                    }
                };
                length += match step {
                    Step::Byte(_) => 1,
                    Step::Back(_, n) | Step::Source(_, n) => n as usize,
                };
                steps.push(step);
            }
            let oracle = expected(&steps, &base);
            assert_eq!(
                apply_indirect_bounded(&encode(&steps, true), &base, oracle.len()).unwrap(),
                oracle
            );
        }
    }

    #[test]
    fn invalid_distances_base_ranges_and_expansion_limits_return_errors() {
        for steps in [
            vec![Step::Back(1, 3)],
            vec![Step::Byte(0x87), Step::Back(0, 2)],
            vec![Step::Byte(0x87), Step::Back(2, 1)],
        ] {
            assert!(decode_direct(&encode(&steps, false)).is_err());
            assert!(apply_indirect(&encode(&steps, true), &[]).is_err());
        }
        for steps in [vec![Step::Source(-1, 1)], vec![Step::Source(3, 2)]] {
            assert!(apply_indirect(&encode(&steps, true), b"test").is_err());
        }
        let bomb = encode(&[Step::Byte(0x87), Step::Back(1, 255)], false);
        for limit in [0, 1, 2, 255] {
            assert!(decode_direct_bounded(&bomb, limit).is_err());
        }
        assert_eq!(decode_direct_bounded(&bomb, 256).unwrap(), vec![0x87; 256]);
    }

    #[test]
    fn byte_and_token_truncations_never_decode_as_complete_streams() {
        let steps = [Step::Byte(0x97), Step::Back(1, 9)];
        for delta in [false, true] {
            let stream = encode(&steps, delta);
            for end in 0..stream.len() {
                assert!(if delta {
                    apply_indirect(&stream[..end], &[])
                } else {
                    decode_direct(&stream[..end])
                }
                .is_err());
            }
            let mut missing_field = stream;
            let bits = u32::from_le_bytes(missing_field[..4].try_into().unwrap()) - 1;
            missing_field[..4].copy_from_slice(&bits.to_le_bytes());
            assert!(if delta {
                apply_indirect(&missing_field, &[])
            } else {
                decode_direct(&missing_field)
            }
            .is_err());
        }
    }
}
