/// BGRA 到 RGBA 转换的测试用例
/// 比较 SIMD 和标量版本的结果一致性

#[cfg(target_os = "macos")]
mod tests {
    use xcap::platform::bgra_to_rgba;

    /// 标量版本的实现，用于测试对比
    fn convert_bgra_to_rgba_scalar_test(src: &[u8]) -> Vec<u8> {
        let pixel_count = src.len() / 4;
        let mut dst = Vec::with_capacity(pixel_count * 4);
        unsafe {
            let dst_ptr: *mut u8 = dst.as_mut_ptr();
            let src_ptr: *const u8 = src.as_ptr();
            // 标量版本：BGRA -> RGBA
            // BGRA: [B, G, R, A]
            // RGBA: [R, G, B, A]
            for i in 0..pixel_count {
                let src_offset = i * 4;
                let dst_offset = i * 4;
                *dst_ptr.add(dst_offset) = *src_ptr.add(src_offset + 2); // R
                *dst_ptr.add(dst_offset + 1) = *src_ptr.add(src_offset + 1); // G
                *dst_ptr.add(dst_offset + 2) = *src_ptr.add(src_offset); // B
                *dst_ptr.add(dst_offset + 3) = *src_ptr.add(src_offset + 3); // A
            }
            dst.set_len(pixel_count * 4);
        }
        dst
    }

    /// 测试 SIMD 和标量版本结果一致性
    fn test_simd_vs_scalar(bgra_data: &[u8]) {
        // 使用 SIMD 版本
        let mut simd_result = Vec::new();
        bgra_to_rgba::convert_bgra_to_rgba_simd(bgra_data, &mut simd_result);

        // 使用标量版本
        let scalar_result = convert_bgra_to_rgba_scalar_test(bgra_data);

        // 比较结果
        assert_eq!(
            simd_result.len(),
            scalar_result.len(),
            "长度不匹配: SIMD={}, Scalar={}",
            simd_result.len(),
            scalar_result.len()
        );

        for (i, (simd, scalar)) in simd_result.iter().zip(scalar_result.iter()).enumerate() {
            if simd != scalar {
                println!("位置 {} 不匹配:", i);
                println!("  SIMD结果: {:?}", &simd_result[i.saturating_sub(4)..(i + 8).min(simd_result.len())]);
                println!("  标量结果: {:?}", &scalar_result[i.saturating_sub(4)..(i + 8).min(scalar_result.len())]);
                println!("  原始数据: {:?}", &bgra_data[(i / 4) * 4..((i / 4) + 2) * 4]);
            }
            assert_eq!(
                simd, scalar,
                "位置 {} 不匹配: SIMD={}, Scalar={}, 原始数据: {:?}",
                i,
                simd,
                scalar,
                &bgra_data[(i / 4) * 4..(i / 4) * 4 + 4]
            );
        }
    }

    #[test]
    fn test_single_pixel() {
        // 单个像素: BGRA = [B, G, R, A]
        // 应该转换为: RGBA = [R, G, B, A]
        let bgra = vec![0x11, 0x22, 0x33, 0xFF]; // B=0x11, G=0x22, R=0x33, A=0xFF
        test_simd_vs_scalar(&bgra);
    }

    #[test]
    fn test_two_pixels() {
        let bgra = vec![
            0x11, 0x22, 0x33, 0xFF, // 像素1: B=0x11, G=0x22, R=0x33, A=0xFF
            0x44, 0x55, 0x66, 0xAA, // 像素2: B=0x44, G=0x55, R=0x66, A=0xAA
        ];
        test_simd_vs_scalar(&bgra);
    }

    #[test]
    fn test_four_pixels() {
        // 4个像素，测试 SSE4.1 路径
        let bgra = vec![
            0x11, 0x22, 0x33, 0xFF, // 像素1
            0x44, 0x55, 0x66, 0xAA, // 像素2
            0x77, 0x88, 0x99, 0xBB, // 像素3
            0xAA, 0xBB, 0xCC, 0xDD, // 像素4
        ];
        test_simd_vs_scalar(&bgra);
    }

    #[test]
    fn test_eight_pixels() {
        // 8个像素，测试 AVX2 路径
        let bgra = vec![
            0x11, 0x22, 0x33, 0xFF, // 像素1
            0x44, 0x55, 0x66, 0xAA, // 像素2
            0x77, 0x88, 0x99, 0xBB, // 像素3
            0xAA, 0xBB, 0xCC, 0xDD, // 像素4
            0x01, 0x02, 0x03, 0x04, // 像素5
            0x05, 0x06, 0x07, 0x08, // 像素6
            0x09, 0x0A, 0x0B, 0x0C, // 像素7
            0x0D, 0x0E, 0x0F, 0x10, // 像素8
        ];
        test_simd_vs_scalar(&bgra);
    }

    #[test]
    fn test_nine_pixels() {
        // 9个像素，测试 AVX2 + 剩余像素处理
        let bgra = vec![
            0x11, 0x22, 0x33, 0xFF, // 像素1
            0x44, 0x55, 0x66, 0xAA, // 像素2
            0x77, 0x88, 0x99, 0xBB, // 像素3
            0xAA, 0xBB, 0xCC, 0xDD, // 像素4
            0x01, 0x02, 0x03, 0x04, // 像素5
            0x05, 0x06, 0x07, 0x08, // 像素6
            0x09, 0x0A, 0x0B, 0x0C, // 像素7
            0x0D, 0x0E, 0x0F, 0x10, // 像素8
            0x20, 0x21, 0x22, 0x23, // 像素9 (剩余)
        ];
        test_simd_vs_scalar(&bgra);
    }

    #[test]
    fn test_large_buffer() {
        // 测试较大的缓冲区（100个像素）
        let mut bgra = Vec::new();
        for i in 0..100 {
            bgra.push((i * 4) as u8);     // B
            bgra.push((i * 4 + 1) as u8); // G
            bgra.push((i * 4 + 2) as u8); // R
            bgra.push((i * 4 + 3) as u8); // A
        }
        test_simd_vs_scalar(&bgra);
    }

    #[test]
    fn test_various_sizes() {
        // 测试各种大小，包括边界情况
        for size in 1..=20 {
            let mut bgra = Vec::new();
            for i in 0..size {
                bgra.push((i * 4) as u8);     // B
                bgra.push((i * 4 + 1) as u8); // G
                bgra.push((i * 4 + 2) as u8); // R
                bgra.push((i * 4 + 3) as u8); // A
            }
            test_simd_vs_scalar(&bgra);
        }
    }
}

