#[derive(Copy, Clone, Debug)]
pub struct BitPacker {
    packing_shuffle: u64,
    unpacking_shuffle_even: u64,
    unpacking_shuffle_odd: u64,
}

impl BitPacker {
    // If `mapping[x] = Some(y)`, then byte `x` should be packed to bit `y`.
    pub const fn new(mapping: [Option<u8>; 8]) -> Self {
        let mut packing_shuffle = 0;
        let mut byte_index = 0;
        loop {
            if let Some(bit_index) = mapping[7 - byte_index] {
                packing_shuffle |= (1 << bit_index) << (byte_index * 8);
            }

            byte_index += 1;
            if byte_index >= 8 {
                break
            }
        }

        let mut unpacking_shuffle_even = 0;
        let mut unpacking_shuffle_odd = 0;
        let mut byte_index = 0;
        loop {
            if let Some(bit_index) = mapping[byte_index] {
                let c = (1 << (7 - bit_index)) << (byte_index * 8);
                if byte_index.is_multiple_of(2) {
                    unpacking_shuffle_even |= c;
                } else {
                    unpacking_shuffle_odd |= c;
                }
            }

            byte_index += 1;
            if byte_index >= 8 {
                break
            }
        }

        Self {
            packing_shuffle,
            unpacking_shuffle_even,
            unpacking_shuffle_odd,
        }
    }

    pub fn pack(&self, val: u64) -> u8 {
        (val.wrapping_mul(self.packing_shuffle) >> 56) as u8
    }

    pub fn unpack(&self, val: u8) -> u64 {
        let x = val as u64;
        ((x.wrapping_mul(self.unpacking_shuffle_even) | x.wrapping_mul(self.unpacking_shuffle_odd)) >> 7) & 0x0101_0101_0101_0101
    }
}

#[cfg(test)]
mod tests {
    use crate::util::packing::BitPacker;

    #[test]
    fn shuffle_const() {
        assert_eq!(
            BitPacker::new([Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7),]).packing_shuffle,
            0x0102_0408_1020_4080
        );

        assert_eq!(
            BitPacker::new([Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), None,]).packing_shuffle,
            0x0102_0408_1020_4000
        );
    }

    fn unpack_bits_slow(n: u8, mapping: &[Option<u8>; 8]) -> u64 {
        mapping
            .iter()
            .enumerate()
            .map(|(b, v)| {
                if let Some(v) = v {
                    ((n >> v) as u64 & 1) << (b * 8)
                } else {
                    0
                }
            })
            .reduce(|a, b| a | b)
            .unwrap()
    }

    #[test]
    fn packing() {
        let mapping = [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7)];
        let bp = BitPacker::new(mapping);

        for n in 0..=255 {
            let bits = unpack_bits_slow(n, &mapping);
            println!("0x{bits:016X} = {:08b}", bp.pack(bits));
            assert_eq!(n, bp.pack(bits));
            assert_eq!(bp.unpack(n), bits);
        }

        let mapping = [Some(0), Some(2), Some(4), Some(6), Some(7), None, None, None];
        let bp = BitPacker::new(mapping);

        for n in 0..=255 {
            let n = n & 0b1101_0101;
            let bits = unpack_bits_slow(n, &mapping);
            println!("0x{bits:016X} = {:08b} = 0x{:016X}", bp.pack(bits), bp.unpack(n));
            assert_eq!(n, bp.pack(bits));
            assert_eq!(bp.unpack(n), bits, "{bp:016X?}");
        }
    }
}
