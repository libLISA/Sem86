pub trait IntoBcd<N> {
    fn to_bcd(&self) -> N;
}

fn convert_into_bcd_internal<const N: usize>(mut val: u64) -> [u8; N] {
    let mut result = [0; N];
    for byte in result.iter_mut().rev() {
        let digit0 = val % 10;
        let digit1 = (val / 10) % 10;
        val /= 100;
        *byte = (digit0 | (digit1 << 4)) as u8;
    }

    result
}

impl IntoBcd<u8> for u8 {
    fn to_bcd(&self) -> u8 {
        convert_into_bcd_internal::<1>(*self as u64)[0]
    }
}
