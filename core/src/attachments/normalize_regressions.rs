use super::*;
use std::sync::atomic::Ordering;

fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = tag.to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    if data.len() % 2 != 0 {
        out.push(0);
    }
    out
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend((4 + chunks.iter().map(Vec::len).sum::<usize>() as u32).to_le_bytes());
    out.extend(b"WEBP");
    for c in chunks {
        out.extend(c);
    }
    out
}

fn vp8x(width: u32, height: u32, flags: u8) -> Vec<u8> {
    let mut header = vec![flags, 0, 0, 0];
    header.extend(&(width - 1).to_le_bytes()[..3]);
    header.extend(&(height - 1).to_le_bytes()[..3]);
    chunk(b"VP8X", &header)
}

fn vp8(width: u16, height: u16) -> Vec<u8> {
    let mut header = vec![0x10, 0, 0, 0x9d, 1, 0x2a];
    header.extend(width.to_le_bytes());
    header.extend(height.to_le_bytes());
    chunk(b"VP8 ", &header)
}

fn vp8l(width: u32, height: u32) -> Vec<u8> {
    let mut header = vec![0x2f];
    header.extend(((width - 1) | ((height - 1) << 14)).to_le_bytes());
    chunk(b"VP8L", &header)
}

fn frame(width: u32, height: u32, chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0; 6];
    out.extend(&(width - 1).to_le_bytes()[..3]);
    out.extend(&(height - 1).to_le_bytes()[..3]);
    out.extend([1, 0, 0, 2]); // One millisecond, no blending.
    for c in chunks {
        out.extend(c);
    }
    chunk(b"ANMF", &out)
}

pub(in crate::attachments) fn panicking_webp() -> Vec<u8> {
    // A complete red 2x2 VP8 keyframe, paired with a one-pixel uncompressed alpha plane.
    let payload = [
        208, 1, 0, 157, 1, 42, 2, 0, 2, 0, 1, 64, 38, 37, 160, 2, 116, 186, 1, 248, 0, 3, 176, 0, 254, 242, 235, 127,
        252, 216, 21, 205, 115, 239, 247, 255, 210, 224, 253, 46, 15, 210, 224, 255, 210, 144, 0, 0,
    ];
    riff(&[
        vp8x(1, 1, 0x12),
        chunk(b"ANIM", &[0; 6]),
        frame(1, 1, &[chunk(b"ALPH", &[0, 255]), chunk(b"VP8 ", &payload)]),
    ])
}

// Disallow the actual pixel decode so a failed preflight cannot allocate from a hostile header.
fn refused_before_decode(raw: &[u8], expected: ImageRefusal) {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ALLOW_DECODE.with(|allow| allow.set(true));
        }
    }
    let _restore = Restore;
    ALLOW_DECODE.with(|allow| allow.set(false));
    DECODE_CALLS.with(|calls| calls.store(0, Ordering::Relaxed));
    assert_eq!(normalize(raw, RAW_MAX_BYTES).err(), Some(expected));
    DECODE_CALLS.with(|calls| assert_eq!(calls.load(Ordering::Relaxed), 0));
}

#[test]
fn webp_canvas_buffers_must_fit_the_decode_allocation_budget() {
    refused_before_decode(
        &riff(&[
            vp8x(6000, 6000, 2),
            chunk(b"ANIM", &[0; 6]),
            frame(6000, 6000, &[vp8l(6000, 6000)]),
        ]),
        ImageRefusal::CanvasBuffersTooLarge,
    );
    for raw in [
        riff(&[vp8l(6000, 6000)]),
        riff(&[vp8x(6000, 6000, 0), vp8l(6000, 6000)]),
    ] {
        refused_before_decode(&raw, ImageRefusal::CanvasBuffersTooLarge);
    }
}

fn gif_descriptor(screen: (u16, u16), origin: (u16, u16), size: (u16, u16)) -> Vec<u8> {
    let mut raw = b"GIF89a".to_vec();
    raw.extend(screen.0.to_le_bytes());
    raw.extend(screen.1.to_le_bytes());
    raw.extend([0, 0, 0, 0x2c]);
    for value in [origin.0, origin.1, size.0, size.1] {
        raw.extend(value.to_le_bytes());
    }
    raw.push(0);
    raw
}

#[test]
fn a_gif_screen_cannot_hide_an_oversized_first_descriptor() {
    let mut raw = gif_descriptor((1, 1), (0, 0), (8000, 8000));
    let expected = ImageRefusal::TooManyPixels {
        width: 8000,
        height: 8000,
    };
    refused_before_decode(&raw, expected.clone());
    // Supply the palette and LZW metadata needed to reach the decoder's frame-buffer allocation,
    // but no compressed pixels. The decode guard keeps a broken preflight safe to test.
    raw[10] = 0x80;
    raw.splice(13..13, [0, 0, 0, 255, 255, 255]);
    raw.extend([2, 1]);
    refused_before_decode(&raw, expected);
}

#[test]
fn a_gif_first_frame_must_fit_its_screen_including_offsets() {
    for (origin, size) in [
        ((0, 0), (3, 1)),
        ((0, 0), (1, 3)),
        ((1, 0), (2, 1)),
        ((0, 1), (1, 2)),
        ((65535, 0), (1, 1)),
    ] {
        refused_before_decode(&gif_descriptor((2, 2), origin, size), ImageRefusal::FrameOutsideScreen);
    }
}

#[test]
fn gif_preflight_rejects_truncated_tables_extensions_and_descriptors() {
    let raw = gif_descriptor((1, 1), (0, 0), (1, 1));
    for end in 6..raw.len() {
        refused_before_decode(&raw[..end], ImageRefusal::Undecodable);
    }
    let mut header = raw[..13].to_vec();
    header[10] = 0x80; // A global table with two entries, neither supplied.
    refused_before_decode(&header, ImageRefusal::Undecodable);
    for tail in [
        &[0x21][..],
        &[0x21, 0xfe],
        &[0x21, 0xfe, 3, 1, 2],
        &[0x21, 0xfe, 1, 0],
        &[0x3b],
        &[0x00],
    ] {
        let mut truncated = raw[..13].to_vec();
        truncated.extend(tail);
        refused_before_decode(&truncated, ImageRefusal::Undecodable);
    }
}

#[test]
fn gif_preflight_skips_global_tables_and_extension_subblocks() {
    let mut raw = tests::gif(1);
    let table_len = if raw[10] & 0x80 != 0 {
        3 * (1 << ((raw[10] & 7) + 1))
    } else {
        0
    };
    let pos = 13 + table_len;
    raw.splice(pos..pos, [0x21, 0xfe, 3, 0x2c, 0x21, 0x3b, 1, 0, 0]);
    assert!(normalize(&raw, RAW_MAX_BYTES).is_ok());
}

#[test]
fn a_webp_canvas_cannot_hide_oversized_vp8_or_vp8l_headers() {
    for embedded in [vp8(16383, 16383), vp8l(16384, 16384)] {
        let side = if &embedded[..4] == b"VP8 " { 16383 } else { 16384 };
        refused_before_decode(
            &riff(&[vp8x(1, 1, 0), embedded]),
            ImageRefusal::TooManyPixels {
                width: side,
                height: side,
            },
        );
    }
}

#[test]
fn every_webp_frame_is_checked_even_after_a_valid_first_frame() {
    for embedded in [vp8(16383, 16383), vp8l(16383, 16383)] {
        refused_before_decode(
            &riff(&[
                vp8x(1, 1, 2),
                chunk(b"ANIM", &[0; 6]),
                frame(1, 1, &[vp8l(1, 1)]),
                frame(1, 1, &[embedded]),
            ]),
            ImageRefusal::TooManyPixels {
                width: 16383,
                height: 16383,
            },
        );
    }
}

#[test]
fn webp_embedded_sizes_must_match_their_frame_and_canvas() {
    for embedded in [vp8(2, 2), vp8l(2, 2)] {
        refused_before_decode(
            &riff(&[vp8x(1, 1, 0), embedded]),
            ImageRefusal::TooManyPixels { width: 2, height: 2 },
        );
    }
    refused_before_decode(&riff(&[vp8x(2, 2, 0), vp8l(1, 1)]), ImageRefusal::Undecodable);
    refused_before_decode(
        &riff(&[vp8x(3, 3, 2), chunk(b"ANIM", &[0; 6]), frame(1, 1, &[vp8(2, 2)])]),
        ImageRefusal::Undecodable,
    );
}

#[test]
fn malformed_webp_chunk_lengths_and_headers_never_reach_decode() {
    let valid = riff(&[vp8x(1, 1, 0), vp8l(1, 1)]);
    let mut oversized_chunk = valid.clone();
    oversized_chunk[34..38].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut bad_riff_size = valid.clone();
    bad_riff_size[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut missing_padding = valid.clone();
    missing_padding.pop();
    let mut frame_overrun = frame(1, 1, &[vp8l(1, 1)]);
    frame_overrun[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
    for raw in [
        oversized_chunk,
        bad_riff_size,
        missing_padding,
        riff(&[vp8x(1, 1, 0), chunk(b"VP8 ", &[0; 9])]),
        riff(&[vp8x(1, 1, 0), chunk(b"VP8L", &[0; 5])]),
        riff(&[vp8x(1, 1, 0), vp8l(1, 1), vp8l(1, 1)]),
        riff(&[vp8x(1, 1, 2), chunk(b"ANIM", &[0; 6]), frame_overrun]),
        riff(&[
            vp8x(1, 1, 2),
            chunk(b"ANIM", &[0; 6]),
            frame(1, 1, &[vp8l(1, 1), vp8l(1, 1)]),
        ]),
    ] {
        refused_before_decode(&raw, ImageRefusal::Undecodable);
    }
}

#[test]
fn jpeg_completion_handles_multiple_scans_stuffing_metadata_and_trailing_data() {
    let scan = [0xff, 0xda, 0, 2, 1, 0xff, 0, 2, 0xff, 0xd0, 3];
    let mut raw = vec![0xff, 0xd8, 0xff, 0xe1, 0, 4, 0xff, 0xd9];
    raw.extend(scan);
    assert_eq!(check_jpeg_complete(&raw), Err(ImageRefusal::Undecodable));
    raw.extend([0xff, 0xc4, 0, 2]);
    raw.extend(scan);
    raw.extend([0xff, 0xff, 0xd9]);
    assert_eq!(check_jpeg_complete(&raw), Ok(()));
    raw.extend(b"trailing application data");
    assert_eq!(check_jpeg_complete(&raw), Ok(()));
}

#[test]
fn oversized_dimensions_are_refused_consistently_for_all_formats() {
    for (width, height) in [(20001, 1), (1, 20001), (10000, 5000)] {
        let mut gif = tests::gif(1);
        gif[6..8].copy_from_slice(&(width as u16).to_le_bytes());
        gif[8..10].copy_from_slice(&(height as u16).to_le_bytes());
        let mut jpeg = encode(
            &image::load_from_memory(&tests::png(1, 1, 255)).unwrap(),
            ImageType::Jpeg,
        )
        .unwrap();
        let sof = jpeg.windows(2).position(|b| b == [0xff, 0xc0]).unwrap();
        jpeg[sof + 5..sof + 7].copy_from_slice(&(height as u16).to_be_bytes());
        jpeg[sof + 7..sof + 9].copy_from_slice(&(width as u16).to_be_bytes());
        for raw in [
            boundary_tests::header(width, height),
            gif,
            jpeg,
            riff(&[vp8x(width, height, 0), vp8l(1, 1)]),
        ] {
            refused_before_decode(&raw, ImageRefusal::TooManyPixels { width, height });
        }
    }
}

#[test]
fn a_decode_allocation_limit_is_reported_as_too_large() {
    refused_before_decode(
        &{
            let mut raw = boundary_tests::header(7000, 5000);
            // RGBA16 needs 280 MB, beyond the allocation limit but within the pixel limit.
            raw[24] = 16;
            // Rebuild the CRC over IHDR after changing the bit depth.
            let mut crc = !0_u32;
            for byte in &raw[12..29] {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(crc & 1));
                }
            }
            raw[29..33].copy_from_slice(&(!crc).to_be_bytes());
            raw
        },
        ImageRefusal::TooLargeToDecode,
    );
}

pub(in crate::attachments) fn oriented_jpeg(width: u32, height: u32) -> Vec<u8> {
    let pixels = image::RgbImage::from_fn(width, height, |x, _| {
        image::Rgb(if x < width / 2 { [255, 0, 0] } else { [0, 0, 255] })
    });
    let jpeg = encode(&DynamicImage::ImageRgb8(pixels), ImageType::Jpeg).unwrap();
    let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut raw = vec![0xff, 0xd8, 0xff, 0xe1];
    raw.extend(((exif.len() + 2) as u16).to_be_bytes());
    raw.extend(exif);
    raw.extend(&jpeg[2..]);
    raw
}

#[test]
fn exif_orientation_is_applied_to_metadata_and_thumbnail_when_original_bytes_are_kept() {
    let raw = oriented_jpeg(40, 20);
    let n = normalize(&raw, RAW_MAX_BYTES).unwrap();
    assert_eq!(n.bytes, raw);
    assert_eq!((n.width, n.height), (20, 40));
    let thumb = image::load_from_memory(&n.thumbnail_png).unwrap().to_rgb8();
    assert_eq!(thumb.dimensions(), (20, 40));
    assert!(thumb.get_pixel(10, 5)[0] > 240);
    assert!(thumb.get_pixel(10, 35)[2] > 240);
}

#[test]
fn exif_orientation_is_applied_before_resizing_and_reencoding() {
    let n = normalize(&oriented_jpeg(1600, 20), RAW_MAX_BYTES).unwrap();
    assert_eq!((n.width, n.height), fitted_size(20, 1600, OUT_MAX_SIDE, OUT_MAX_AREA));
    let image = image::load_from_memory(&n.bytes).unwrap().to_rgb8();
    assert_eq!(image.dimensions(), (n.width, n.height));
    assert!(image.get_pixel(n.width / 2, 5)[0] > 240);
    assert!(image.get_pixel(n.width / 2, n.height - 6)[2] > 240);
}

#[test]
fn sixteen_bit_png_output_and_thumbnails_are_encoded_as_eight_bit() {
    for alpha in [u16::MAX, 32768] {
        let image = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            1600,
            2,
            image::Rgba([50000, 30000, 10000, alpha]),
        ));
        let mut raw = Cursor::new(Vec::new());
        image.write_to(&mut raw, ImageFormat::Png).unwrap();
        let n = normalize(raw.get_ref(), RAW_MAX_BYTES).unwrap();
        assert_eq!(
            image::load_from_memory(&n.bytes).unwrap().color().bits_per_pixel(),
            if alpha == u16::MAX { 24 } else { 32 }
        );
        assert_eq!(
            image::load_from_memory(&n.thumbnail_png)
                .unwrap()
                .color()
                .bits_per_pixel(),
            32
        );
    }
    let image = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
        2,
        1,
        image::Rgba([50000, 30000, 10000, 32768]),
    ));
    let mut raw = Cursor::new(Vec::new());
    image.write_to(&mut raw, ImageFormat::Png).unwrap();
    let n = normalize(raw.get_ref(), RAW_MAX_BYTES).unwrap();
    assert_eq!(&n.bytes, raw.get_ref());
    assert_eq!(
        image::load_from_memory(&n.thumbnail_png)
            .unwrap()
            .color()
            .bits_per_pixel(),
        32
    );
}

#[test]
fn an_animated_webp_uses_only_its_first_frame() {
    let frame_chunk = |rgba| {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 2, image::Rgba(rgba)));
        let mut raw = Cursor::new(Vec::new());
        image.write_to(&mut raw, ImageFormat::WebP).unwrap();
        // The encoder emits a simple WebP containing one VP8L chunk.
        frame(2, 2, &[raw.into_inner()[12..].to_vec()])
    };
    let raw = riff(&[
        vp8x(2, 2, 2),
        chunk(b"ANIM", &[0; 6]),
        frame_chunk([255, 0, 0, 255]),
        frame_chunk([0, 0, 255, 255]),
    ]);
    let n = normalize(&raw, RAW_MAX_BYTES).unwrap();
    let pixel = image::load_from_memory(&n.bytes).unwrap().to_rgb8().get_pixel(0, 0).0;
    assert!(pixel[0] > 240 && pixel[1] < 10 && pixel[2] < 10);
}

#[test]
fn the_resized_size_refusal_names_the_actual_limit_and_byte_count() {
    let raw = tests::png(2, 1, 255);
    let refusal = normalize(&raw, 17).err().unwrap();
    assert_eq!(
        refusal.sentence(),
        format!(
            "image still too large after resizing: {} bytes (the limit is 17 bytes)",
            raw.len()
        )
    );
}

#[test]
fn images_without_an_alpha_channel_do_not_scan_pixels_for_transparency() {
    ALPHA_SCAN_CALLS.with(|calls| calls.set(0));
    normalize(&oriented_jpeg(1600, 20), RAW_MAX_BYTES).unwrap();
    ALPHA_SCAN_CALLS.with(|calls| assert_eq!(calls.get(), 0));
    normalize(&tests::png(1600, 2, 100), RAW_MAX_BYTES).unwrap();
    ALPHA_SCAN_CALLS.with(|calls| assert_eq!(calls.get(), 1));
}

// A lossless (VP8L) bitstream writer: bits go in least-significant first, Huffman codes most-significant first.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    count: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= u64::from(value) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.count -= 8;
        }
    }

    fn code(&mut self, code: u32, len: u32) {
        for bit in (0..len).rev() {
            self.put((code >> bit) & 1, 1);
        }
    }

    fn simple_code(&mut self, symbol: u8) {
        // One symbol, written in eight bits; reading it costs no bits.
        self.put(1, 1);
        self.put(0, 1);
        self.put(1, 1);
        self.put(u32::from(symbol), 8);
    }

    // The cheapest complete code whose longest length is 10, which gives it a full 1024-entry lookup table:
    // symbols 0..=10 with lengths 1, 2, ..., 9, 10, 10 (about 93 bits).
    fn ten_bit_code(&mut self) {
        const ORDER: [usize; 14] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10];
        const LENGTHS: [u32; 11] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10];
        // The code-length code: lengths 1..=6 take three bits (codes 0..=5), 7..=10 four (codes 12..=15).
        let code_length_bits = |symbol: usize| match symbol {
            1..=6 => 3,
            7..=10 => 4,
            _ => 0,
        };
        self.put(0, 1);
        self.put(ORDER.len() as u32 - 4, 4);
        for symbol in ORDER {
            self.put(code_length_bits(symbol), 3);
        }
        // Only the first eleven symbols are read; the rest stay zero.
        self.put(1, 1);
        self.put(1, 3);
        self.put(LENGTHS.len() as u32 - 2, 4);
        for length in LENGTHS {
            let code = if length <= 6 { length - 1 } else { length + 5 };
            self.code(code, code_length_bits(length as usize));
        }
    }
}

/// A 1x1 lossless WebP whose entropy image names prefix-code group `groups - 1`, followed by that many groups of five
/// ten-bit codes. The format makes a decoder read every group up to the highest one named; image-webp keeps them all.
pub(in crate::attachments) fn vp8l_with_prefix_groups(groups: u32) -> Vec<u8> {
    assert!((1..=65536).contains(&groups));
    let mut bits = Bits {
        out: Vec::new(),
        acc: 0,
        count: 0,
    };
    bits.put(0x2f, 8);
    bits.put(0, 14);
    bits.put(0, 14);
    bits.put(0, 1);
    bits.put(0, 3);
    bits.put(0, 1); // No transforms.
    bits.put(0, 1); // No colour cache.
    bits.put(1, 1); // An entropy image, one pixel per 4x4 block.
    bits.put(0, 3);
    bits.put(0, 1); // The entropy image: no colour cache, one single-symbol code per channel.
    let highest = groups - 1;
    bits.simple_code(highest as u8); // Green: the low byte of the group number.
    bits.simple_code((highest >> 8) as u8); // Red: the high byte.
    for _ in 0..3 {
        bits.simple_code(0);
    }
    for _ in 0..groups * 5 {
        bits.ten_bit_code();
    }
    // The pixel: green, red, blue and alpha are each symbol 0, the one-bit code 0.
    bits.put(0, 4);
    bits.put(0, 32);
    let mut payload = bits.out;
    if bits.count > 0 {
        payload.push(bits.acc as u8);
    }
    riff(&[chunk(b"VP8L", &payload)])
}

#[test]
fn the_test_vp8l_writer_produces_a_decodable_image() {
    for groups in [1, 300] {
        let n = normalize(&vp8l_with_prefix_groups(groups), RAW_MAX_BYTES).unwrap();
        assert_eq!((n.width, n.height), (1, 1));
    }
    // 65536 groups of five codes, each about 93 bits, fit well under the raw size limit.
    let hostile = vp8l_with_prefix_groups(65536).len() as u64;
    assert!(hostile > 3_000_000 && hostile < RAW_MAX_BYTES);
}

#[test]
fn an_offscreen_gif_and_a_webp_canvas_budget_have_distinct_refusals() {
    let gif = gif_descriptor((2, 2), (1, 0), (2, 1));
    assert_eq!(
        normalize(&gif, RAW_MAX_BYTES).err().unwrap().sentence(),
        "this GIF's frame extends outside its logical screen"
    );
    let webp = riff(&[vp8l(6000, 6000)]);
    let reason = normalize(&webp, RAW_MAX_BYTES).err().unwrap().sentence();
    assert_eq!(
        reason,
        "this WebP's canvas buffers exceed the decode memory limit (256 MB)"
    );
}
