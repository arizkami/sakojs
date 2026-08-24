// SPDX-License-Identifier: BSD-3-Clause

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Implementation {
    Scalar,
    Sse42,
    Avx2,
}

pub fn implementation() -> Implementation {
    static SELECTED: OnceLock<Implementation> = OnceLock::new();
    *SELECTED.get_or_init(|| {
        let requested = std::env::var("SAKO_ACCEL_FORCE").unwrap_or_default();
        match requested.to_ascii_lowercase().as_str() {
            "scalar" => Implementation::Scalar,
            "sse42" if std::arch::is_x86_feature_detected!("sse4.2") => Implementation::Sse42,
            "avx2" if std::arch::is_x86_feature_detected!("avx2") => Implementation::Avx2,
            _ if std::arch::is_x86_feature_detected!("avx2") => Implementation::Avx2,
            _ => Implementation::Scalar,
        }
    })
}

pub fn find_byte(source: &[u8], needle: u8) -> Option<usize> {
    match implementation() {
        Implementation::Scalar => find_byte_scalar(source, needle),
        Implementation::Sse42 => {
            // SAFETY: selection only returns SSE4.2 after runtime detection.
            unsafe { find_byte_sse42(source, needle) }
        }
        Implementation::Avx2 => {
            // SAFETY: selection only returns AVX2 after runtime detection.
            unsafe { find_byte_avx2(source, needle) }
        }
    }
}

#[target_feature(enable = "sse4.2")]
unsafe fn find_byte_sse42(source: &[u8], needle: u8) -> Option<usize> {
    use std::arch::x86_64::{
        __m128i, _SIDD_CMP_EQUAL_ANY, _SIDD_LEAST_SIGNIFICANT, _SIDD_UBYTE_OPS, _mm_cmpestri,
        _mm_loadu_si128, _mm_set1_epi8,
    };

    let target = _mm_set1_epi8(needle as i8);
    const MODE: i32 = _SIDD_UBYTE_OPS | _SIDD_CMP_EQUAL_ANY | _SIDD_LEAST_SIGNIFICANT;
    let mut offset = 0;
    while offset + 16 <= source.len() {
        // SAFETY: the loop condition guarantees 16 readable bytes and loadu
        // permits arbitrary alignment.
        let block = unsafe { _mm_loadu_si128(source.as_ptr().add(offset).cast::<__m128i>()) };
        let index = _mm_cmpestri(target, 1, block, 16, MODE);
        if index < 16 {
            return Some(offset + index as usize);
        }
        offset += 16;
    }
    find_byte_scalar(&source[offset..], needle).map(|index| offset + index)
}

pub fn find_sequence(source: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > source.len() {
        return None;
    }
    let mut offset = 0;
    while offset <= source.len() - needle.len() {
        let relative = find_byte(&source[offset..=source.len() - needle.len()], needle[0])?;
        let candidate = offset + relative;
        if source[candidate..].starts_with(needle) {
            return Some(candidate);
        }
        offset = candidate + 1;
    }
    None
}

fn find_byte_scalar(source: &[u8], needle: u8) -> Option<usize> {
    source.iter().position(|byte| *byte == needle)
}

#[target_feature(enable = "avx2")]
unsafe fn find_byte_avx2(source: &[u8], needle: u8) -> Option<usize> {
    use std::arch::x86_64::{
        __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
    };

    let target = _mm256_set1_epi8(needle as i8);
    let mut offset = 0;
    while offset + 32 <= source.len() {
        // SAFETY: the loop condition guarantees 32 readable bytes beginning at
        // offset. loadu permits arbitrary alignment.
        let block = unsafe { _mm256_loadu_si256(source.as_ptr().add(offset).cast::<__m256i>()) };
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, target)) as u32;
        if mask != 0 {
            return Some(offset + mask.trailing_zeros() as usize);
        }
        offset += 32;
    }
    find_byte_scalar(&source[offset..], needle).map(|index| offset + index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_byte_scanner_matches_scalar_for_lengths_and_offsets() {
        for length in 0..257 {
            for position in 0..=length {
                let mut source = vec![b'a'; length];
                if position < length {
                    source[position] = b'\r';
                }
                assert_eq!(
                    find_byte(&source, b'\r'),
                    find_byte_scalar(&source, b'\r'),
                    "length={length}, position={position}"
                );
                if std::arch::is_x86_feature_detected!("sse4.2") {
                    // SAFETY: this branch performs the required feature check.
                    assert_eq!(
                        unsafe { find_byte_sse42(&source, b'\r') },
                        find_byte_scalar(&source, b'\r'),
                        "SSE4.2 length={length}, position={position}"
                    );
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    // SAFETY: this branch performs the required feature check.
                    assert_eq!(
                        unsafe { find_byte_avx2(&source, b'\r') },
                        find_byte_scalar(&source, b'\r'),
                        "AVX2 length={length}, position={position}"
                    );
                }
            }
        }
    }

    #[test]
    fn sequence_scanner_handles_boundaries_and_absence() {
        for prefix in 0..96 {
            let mut source = vec![b'x'; prefix];
            source.extend_from_slice(b"\r\n\r\nbody");
            assert_eq!(find_sequence(&source, b"\r\n\r\n"), Some(prefix));
        }
        assert_eq!(find_sequence(b"partial\r\n\r", b"\r\n\r\n"), None);
    }
}
