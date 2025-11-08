/// BGRA 到 RGBA 像素格式转换模块
/// 使用 SIMD 优化，支持 x86_64 (SSE/AVX) 和 arm64 (NEON)

/// 使用 SIMD 优化 BGRA -> RGBA 转换
/// 支持 x86_64 (SSE/AVX) 和 arm64 (NEON)
#[inline]
pub fn convert_bgra_to_rgba_simd(src: &[u8], dst: &mut Vec<u8>) {
    let pixel_count = src.len() / 4;
    dst.reserve_exact(pixel_count * 4);

    unsafe {
        let dst_ptr = dst.as_mut_ptr();
        let src_ptr = src.as_ptr();

        #[cfg(target_arch = "x86_64")]
        {
            // x86_64: 使用 SSE/AVX，运行时检测 CPU 特性
            if std::arch::is_x86_feature_detected!("avx2") {
                convert_bgra_to_rgba_avx2(src_ptr, dst_ptr, pixel_count);
            } else if std::arch::is_x86_feature_detected!("sse4.1") {
                convert_bgra_to_rgba_sse41(src_ptr, dst_ptr, pixel_count);
            } else {
                convert_bgra_to_rgba_scalar(src_ptr, dst_ptr, pixel_count);
            }
        }

        #[cfg(target_arch = "aarch64")]
        {
            // ARM64: 使用 NEON
            convert_bgra_to_rgba_neon(src_ptr, dst_ptr, pixel_count);
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            // 回退到标量版本
            convert_bgra_to_rgba_scalar(src_ptr, dst_ptr, pixel_count);
        }

        dst.set_len(pixel_count * 4);
    }
}

/// 标量版本：逐像素转换 BGRA -> RGBA
#[inline]
unsafe fn convert_bgra_to_rgba_scalar(src: *const u8, dst: *mut u8, pixel_count: usize) {
    for i in 0..pixel_count {
        let src_offset = i * 4;
        let dst_offset = i * 4;
        *dst.add(dst_offset) = *src.add(src_offset + 2); // R
        *dst.add(dst_offset + 1) = *src.add(src_offset + 1); // G
        *dst.add(dst_offset + 2) = *src.add(src_offset); // B
        *dst.add(dst_offset + 3) = *src.add(src_offset + 3); // A
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn convert_bgra_to_rgba_avx2(src: *const u8, dst: *mut u8, pixel_count: usize) {
    use std::arch::x86_64::*;

    // AVX2 可以一次处理 8 个像素（32 字节）
    let simd_count = pixel_count / 8;
    let remainder = pixel_count % 8;

    // BGRA 到 RGBA 的 shuffle mask: [2,1,0,3, 6,5,4,7, 10,9,8,11, 14,13,12,15, ...]
    // 对于 256 位（8 个像素），需要两个 128 位的 shuffle
    let mask_lo = _mm256_setr_epi8(
        2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11,
        14, 13, 12, 15,
    );

    for i in 0..simd_count {
        let offset = i * 32;
        let data = _mm256_loadu_si256((src.add(offset) as *const __m256i));
        let shuffled = _mm256_shuffle_epi8(data, mask_lo);
        _mm256_storeu_si256((dst.add(offset) as *mut __m256i), shuffled);
    }

    // 处理剩余像素
    if remainder > 0 {
        convert_bgra_to_rgba_scalar(
            src.add(simd_count * 32),
            dst.add(simd_count * 32),
            remainder,
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn convert_bgra_to_rgba_sse41(src: *const u8, dst: *mut u8, pixel_count: usize) {
    use std::arch::x86_64::*;

    // SSE4.1 可以一次处理 4 个像素（16 字节）
    let simd_count = pixel_count / 4;
    let remainder = pixel_count % 4;

    // BGRA 到 RGBA 的 shuffle mask: [2,1,0,3, 6,5,4,7, 10,9,8,11, 14,13,12,15]
    let mask = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);

    for i in 0..simd_count {
        let offset = i * 16;
        let data = _mm_loadu_si128((src.add(offset) as *const __m128i));
        let shuffled = _mm_shuffle_epi8(data, mask);
        _mm_storeu_si128((dst.add(offset) as *mut __m128i), shuffled);
    }

    // 处理剩余像素
    if remainder > 0 {
        convert_bgra_to_rgba_scalar(
            src.add(simd_count * 16),
            dst.add(simd_count * 16),
            remainder,
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn convert_bgra_to_rgba_neon(src: *const u8, dst: *mut u8, pixel_count: usize) {
    use std::arch::aarch64::*;

    // NEON 可以一次处理 4 个像素（16 字节）
    let simd_count = pixel_count / 4;
    let remainder = pixel_count % 4;

    // BGRA 到 RGBA: 使用 tbl (table lookup) 指令
    // 索引 mask: [2,1,0,3, 6,5,4,7, 10,9,8,11, 14,13,12,15]
    let mask = vcombine_u8(
        vcreate_u8(0x0300010207060504),
        vcreate_u8(0x0B0A09080F0E0D0C),
    );

    for i in 0..simd_count {
        let offset = i * 16;
        let data = vld1q_u8(src.add(offset));
        // 使用 tbl 进行字节重排
        let shuffled = vqtbl1q_u8(data, mask);
        vst1q_u8(dst.add(offset), shuffled);
    }

    // 处理剩余像素
    if remainder > 0 {
        convert_bgra_to_rgba_scalar(
            src.add(simd_count * 16),
            dst.add(simd_count * 16),
            remainder,
        );
    }
}

/// 对单行数据进行 BGRA -> RGBA 转换（用于非对齐情况）
/// 支持 SIMD 优化
#[inline]
pub unsafe fn convert_bgra_to_rgba_row(
    src: *const u8,
    dst: *mut u8,
    pixel_count: usize,
) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            convert_bgra_to_rgba_avx2(src, dst, pixel_count);
        } else if std::arch::is_x86_feature_detected!("sse4.1") {
            convert_bgra_to_rgba_sse41(src, dst, pixel_count);
        } else {
            convert_bgra_to_rgba_scalar(src, dst, pixel_count);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        convert_bgra_to_rgba_neon(src, dst, pixel_count);
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        convert_bgra_to_rgba_scalar(src, dst, pixel_count);
    }
}

