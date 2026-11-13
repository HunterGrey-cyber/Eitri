use super::*;
use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader};
use sha2::{Digest, Sha256};
use std::io::Cursor;

#[cfg(test)]
thread_local! {
    // Per-test-thread storage keeps simultaneous normalizations from contaminating the header check.
    static INPUT_BYTES: std::sync::atomic::AtomicUsize = const { std::sync::atomic::AtomicUsize::new(0) };
    static DECODE_CALLS: std::sync::atomic::AtomicUsize = const { std::sync::atomic::AtomicUsize::new(0) };
    // Malicious-header tests must fail safely even when the preflight is broken.
    static ALLOW_DECODE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static ALPHA_SCAN_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct CountedInput<'a>(Cursor<&'a [u8]>);

#[cfg(test)]
impl std::io::Read for CountedInput<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        // Short reads avoid charging decoder read-ahead as work on later animation frames.
        let len = buffer.len().min(64);
        let read = std::io::Read::read(&mut self.0, &mut buffer[..len])?;
        INPUT_BYTES.with(|bytes| bytes.fetch_add(read, std::sync::atomic::Ordering::Relaxed));
        Ok(read)
    }
}

#[cfg(test)]
impl std::io::BufRead for CountedInput<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let bytes = std::io::BufRead::fill_buf(&mut self.0)?;
        Ok(&bytes[..bytes.len().min(64)])
    }
    fn consume(&mut self, amount: usize) {
        INPUT_BYTES.with(|bytes| bytes.fetch_add(amount, std::sync::atomic::Ordering::Relaxed));
        std::io::BufRead::consume(&mut self.0, amount);
    }
}

#[cfg(test)]
impl std::io::Seek for CountedInput<'_> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        std::io::Seek::seek(&mut self.0, position)
    }
}

fn reader(raw: &[u8], kind: ImageType) -> ImageReader<impl std::io::BufRead + std::io::Seek + '_> {
    let format = match kind {
        ImageType::Png => ImageFormat::Png,
        ImageType::Jpeg => ImageFormat::Jpeg,
        ImageType::Gif => ImageFormat::Gif,
        ImageType::Webp => ImageFormat::WebP,
    };
    #[cfg(test)]
    let input = CountedInput(Cursor::new(raw));
    #[cfg(not(test))]
    let input = Cursor::new(raw);
    let mut reader = ImageReader::with_format(input, format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    reader.limits(limits);
    reader
}

pub fn normalize(raw: &[u8], max_out_bytes: u64) -> Result<Normalized, ImageRefusal> {
    if raw.is_empty() {
        return Err(ImageRefusal::Empty);
    }
    if raw.len() as u64 > RAW_MAX_BYTES {
        return Err(ImageRefusal::TooLargeRaw {
            bytes: raw.len() as u64,
        });
    }
    let kind = sniff(raw).ok_or_else(|| {
        if image::guess_format(raw).is_ok() {
            ImageRefusal::UnsupportedType
        } else {
            ImageRefusal::NotAnImage
        }
    })?;
    if kind == ImageType::Webp {
        super::webp::preflight(raw)?;
    }
    if kind == ImageType::Gif {
        super::gif::preflight(raw)?;
    }
    // PNG enforces allocation limits while constructing its decoder. Read IHDR first so dimension
    // refusals have the same meaning across formats, including when no pixel data follows.
    if kind == ImageType::Png && raw.len() >= 24 && raw[8..16] == *b"\0\0\0\rIHDR" {
        check_dimensions((
            u32::from_be_bytes(raw[16..20].try_into().unwrap()),
            u32::from_be_bytes(raw[20..24].try_into().unwrap()),
        ))?;
    }
    if kind == ImageType::Jpeg {
        check_jpeg_complete(raw)?;
    }
    let mut decoder = reader(raw, kind).into_decoder().map_err(decode_refusal)?;
    check_dimensions(decoder.dimensions())?;
    // from_decoder does not reserve its output buffer. Account for it before allowing internal
    // decoder allocations, just as ImageReader::decode does.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    limits.reserve(decoder.total_bytes()).map_err(decode_refusal)?;
    decoder.set_limits(limits).map_err(decode_refusal)?;
    let orientation = decoder.orientation().map_err(decode_refusal)?;
    #[cfg(test)]
    DECODE_CALLS.with(|calls| calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    #[cfg(test)]
    if !ALLOW_DECODE.with(std::cell::Cell::get) {
        return Err(ImageRefusal::Undecodable);
    }
    // A single-image decode uses only the first GIF or WebP frame.
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(decode_refusal)?;
    decoded.apply_orientation(orientation);
    let (width, height) = decoded.dimensions();
    let (out_width, out_height) = fitted_size(width, height, OUT_MAX_SIDE, OUT_MAX_AREA);
    let keep = (width, height) == (out_width, out_height) && matches!(kind, ImageType::Png | ImageType::Jpeg);
    let has_alpha = !keep && has_transparency(&decoded);
    let image = if (width, height) == (out_width, out_height) {
        decoded
    } else {
        decoded.resize_exact(out_width, out_height, FilterType::Lanczos3)
    };
    let image_type = if keep {
        kind
    } else if has_alpha {
        ImageType::Png
    } else {
        ImageType::Jpeg
    };
    let bytes = if keep {
        raw.to_vec()
    } else {
        encode(&image, image_type)?
    };
    if bytes.len() as u64 > max_out_bytes {
        return Err(ImageRefusal::TooLargeAfterResize {
            bytes: bytes.len() as u64,
            limit: max_out_bytes,
        });
    }
    let mut side = THUMB_MAX_SIDE;
    let thumbnail_png = loop {
        let (tw, th) = fitted_size(out_width, out_height, side, u64::MAX);
        let thumbnail = image.resize_exact(tw, th, FilterType::Lanczos3);
        let png = encode(&thumbnail, ImageType::Png)?;
        if png.len() <= THUMB_MAX_BYTES {
            break png;
        }
        side = (side / 2).max(1);
    };
    Ok(Normalized {
        image_type,
        width: out_width,
        height: out_height,
        sha256: Sha256::digest(&bytes).into(),
        bytes,
        thumbnail_png,
    })
}

pub(super) fn check_dimensions((width, height): (u32, u32)) -> Result<(), ImageRefusal> {
    if width == 0 || height == 0 {
        return Err(ImageRefusal::Undecodable);
    }
    if width > 20_000 || height > 20_000 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ImageRefusal::TooManyPixels { width, height });
    }
    Ok(())
}

fn decode_refusal(error: image::ImageError) -> ImageRefusal {
    match error {
        image::ImageError::Limits(_) => ImageRefusal::TooLargeToDecode,
        _ => ImageRefusal::Undecodable,
    }
}

// The JPEG decoder tolerates incomplete scans. Walk markers to require an end marker outside
// metadata and stuffed scan bytes, while allowing progressive scans and trailing application data.
fn check_jpeg_complete(raw: &[u8]) -> Result<(), ImageRefusal> {
    let mut pos = 2;
    let mut in_scan = false;
    let mut saw_scan = false;
    while pos < raw.len() {
        if in_scan && raw[pos] != 0xff {
            pos += 1;
            continue;
        }
        if raw[pos] != 0xff {
            return Err(ImageRefusal::Undecodable);
        }
        while raw.get(pos) == Some(&0xff) {
            pos += 1;
        }
        let marker = *raw.get(pos).ok_or(ImageRefusal::Undecodable)?;
        pos += 1;
        if in_scan && (marker == 0 || (0xd0..=0xd7).contains(&marker)) {
            continue;
        }
        if marker == 0xd9 {
            return if saw_scan {
                Ok(())
            } else {
                Err(ImageRefusal::Undecodable)
            };
        }
        if marker == 0 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) {
            return Err(ImageRefusal::Undecodable);
        }
        if marker == 1 {
            continue;
        }
        let len = raw.get(pos..pos + 2).ok_or(ImageRefusal::Undecodable)?;
        let len = usize::from(u16::from_be_bytes(len.try_into().unwrap()));
        if len < 2 || len > raw.len() - pos {
            return Err(ImageRefusal::Undecodable);
        }
        pos += len;
        in_scan = marker == 0xda;
        saw_scan |= in_scan;
    }
    Err(ImageRefusal::Undecodable)
}

fn fitted_size(width: u32, height: u32, max_side: u32, max_area: u64) -> (u32, u32) {
    let scale = 1.0_f64
        .min(f64::from(max_side) / f64::from(width.max(height)))
        .min((max_area as f64 / (f64::from(width) * f64::from(height))).sqrt());
    (
        (f64::from(width) * scale).floor().max(1.0) as u32,
        (f64::from(height) * scale).floor().max(1.0) as u32,
    )
}

fn has_transparency(image: &DynamicImage) -> bool {
    if !image.color().has_alpha() {
        return false;
    }
    #[cfg(test)]
    ALPHA_SCAN_CALLS.with(|calls| calls.set(calls.get() + 1));
    image.pixels().any(|(_, _, pixel)| pixel.0[3] < 255)
}

fn encode(image: &DynamicImage, kind: ImageType) -> Result<Vec<u8>, ImageRefusal> {
    let mut bytes = Cursor::new(Vec::new());
    if kind == ImageType::Jpeg {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
            .encode_image(&image.to_rgb8())
            .map_err(|_| ImageRefusal::Undecodable)?;
    } else {
        DynamicImage::ImageRgba8(image.to_rgba8())
            .write_to(&mut bytes, ImageFormat::Png)
            .map_err(|_| ImageRefusal::Undecodable)?;
    }
    Ok(bytes.into_inner())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    pub(crate) fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(width, height, Rgba([90, 120, 180, alpha])));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn normalize_keeps_a_small_png_byte_for_byte() {
        let raw = png(12, 7, 255);
        let n = normalize(&raw, RAW_MAX_BYTES).unwrap();
        assert_eq!(n.bytes, raw);
        assert_eq!((n.width, n.height, n.image_type), (12, 7, ImageType::Png));
    }

    #[test]
    fn normalize_resizes_a_3000x2000_png_to_1568_long_side_within_the_area() {
        let n = normalize(&png(3000, 2000, 255), RAW_MAX_BYTES).unwrap();
        // The area constraint is tighter than the side constraint for a 3:2 image.
        assert!(n.width <= OUT_MAX_SIDE && n.height <= OUT_MAX_SIDE);
        assert!(u64::from(n.width) * u64::from(n.height) <= OUT_MAX_AREA);
        assert!(n.width >= 1300 && n.height >= 870);
    }

    #[test]
    fn an_opaque_png_over_the_bounds_becomes_jpeg_and_one_with_alpha_stays_png() {
        for (alpha, kind) in [(255, ImageType::Jpeg), (100, ImageType::Png)] {
            let n = normalize(&png(1600, 2, alpha), RAW_MAX_BYTES).unwrap();
            assert_eq!(n.image_type, kind);
            assert_eq!(sniff(&n.bytes), Some(kind));
        }
    }

    pub(super) fn gif(frames: usize) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut raw);
            for i in 0..frames {
                encoder
                    .encode_frame(image::Frame::new(RgbaImage::from_pixel(
                        1,
                        1,
                        Rgba([if i == 0 { 255 } else { 0 }, 0, 0, 255]),
                    )))
                    .unwrap();
            }
        }
        raw
    }

    #[test]
    fn a_gif_is_its_first_frame() {
        let n = normalize(
            include_bytes!("../../tests/fixtures/attachments/two-frames.gif"),
            RAW_MAX_BYTES,
        )
        .unwrap();
        let pixel = image::load_from_memory(&n.bytes).unwrap().to_rgb8().get_pixel(0, 0).0;
        assert!(pixel[0] > 240 && pixel[1] < 10 && pixel[2] < 10);
    }

    #[test]
    fn a_gif_with_many_frames_decodes_only_the_first() {
        use std::sync::atomic::Ordering;
        INPUT_BYTES.with(|bytes| bytes.store(0, Ordering::Relaxed));
        let short = normalize(&gif(2), RAW_MAX_BYTES).unwrap();
        let short_reads = INPUT_BYTES.with(|bytes| bytes.load(Ordering::Relaxed));
        let mut long_raw = gif(10_000);
        // A broken tail cannot matter when only the first frame is decoded.
        long_raw.truncate(long_raw.len() - 5);
        INPUT_BYTES.with(|bytes| bytes.store(0, Ordering::Relaxed));
        DECODE_CALLS.with(|calls| calls.store(0, Ordering::Relaxed));
        let long = normalize(&long_raw, RAW_MAX_BYTES).unwrap();
        assert_eq!(short.bytes, long.bytes);
        DECODE_CALLS.with(|calls| assert_eq!(calls.load(Ordering::Relaxed), 1));
        INPUT_BYTES.with(|bytes| {
            assert!(
                bytes.load(Ordering::Relaxed) <= short_reads + 128,
                "decoding must not read through the later 9,999 frames"
            )
        });
    }

    #[test]
    fn webp_decodes_and_is_reencoded() {
        let raw = include_bytes!("../../tests/fixtures/attachments/transparent.webp");
        assert_eq!(sniff(raw), Some(ImageType::Webp));
        let n = normalize(raw, RAW_MAX_BYTES).unwrap();
        assert_eq!(n.image_type, ImageType::Png);
        assert_eq!((n.width, n.height), (4, 3));
    }

    #[test]
    fn text_is_not_an_image_and_png_bytes_are_accepted() {
        assert!(matches!(normalize(b"hello.png", 100), Err(ImageRefusal::NotAnImage)));
        assert!(normalize(&png(1, 1, 255), RAW_MAX_BYTES).is_ok());
    }

    #[test]
    fn an_empty_file_is_refused() {
        assert!(matches!(normalize(b"", 100), Err(ImageRefusal::Empty)));
    }

    #[test]
    fn a_truncated_jpeg_is_undecodable_not_a_panic() {
        let image = image::load_from_memory(&png(32, 16, 255)).unwrap();
        let mut raw = encode(&image, ImageType::Jpeg).unwrap();
        let scan = raw.windows(2).position(|b| b == [0xff, 0xda]).unwrap();
        let scan_start = scan + 2 + usize::from(u16::from_be_bytes([raw[scan + 2], raw[scan + 3]]));
        raw.truncate(scan_start + 1);
        assert!(matches!(normalize(&raw, RAW_MAX_BYTES), Err(ImageRefusal::Undecodable)));
    }
}

#[cfg(test)]
#[path = "normalize_regressions.rs"]
pub(super) mod regressions;

#[cfg(test)]
pub(super) mod boundary_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    pub(in crate::attachments) fn header(width: u32, height: u32) -> Vec<u8> {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        let mut crc = !0_u32;
        for byte in &ihdr {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        let mut raw = b"\x89PNG\r\n\x1a\n".to_vec();
        raw.extend(13_u32.to_be_bytes());
        raw.extend(ihdr);
        raw.extend((!crc).to_be_bytes());
        // No pixels: just the next chunk's header so the PNG reader can finish reading IHDR.
        raw.extend(0_u32.to_be_bytes());
        raw.extend(b"IDAT");
        raw
    }

    #[test]
    fn a_header_over_40_megapixels_is_refused_before_decoding() {
        DECODE_CALLS.with(|calls| calls.store(0, Ordering::Relaxed));
        assert!(matches!(
            normalize(&header(10000, 5000), RAW_MAX_BYTES),
            Err(ImageRefusal::TooManyPixels {
                width: 10000,
                height: 5000
            })
        ));
        DECODE_CALLS.with(|calls| assert_eq!(calls.load(Ordering::Relaxed), 0));
    }

    #[test]
    fn raw_and_encoded_size_caps_are_enforced() {
        let raw = vec![0; RAW_MAX_BYTES as usize + 1];
        assert!(matches!(
            normalize(&raw, u64::MAX),
            Err(ImageRefusal::TooLargeRaw { .. })
        ));
        let raw = tests::png(4, 2, 255);
        assert!(matches!(
            normalize(&raw, raw.len() as u64 - 1),
            Err(ImageRefusal::TooLargeAfterResize { .. })
        ));
    }

    #[test]
    fn a_small_jpeg_is_preserved_and_its_digest_matches() {
        let image = image::load_from_memory(&tests::png(12, 7, 255)).unwrap();
        let raw = encode(&image, ImageType::Jpeg).unwrap();
        let n = normalize(&raw, RAW_MAX_BYTES).unwrap();
        assert_eq!(n.bytes, raw);
        assert_eq!(n.sha256, <[u8; 32]>::from(Sha256::digest(&raw)));
    }

    #[test]
    fn a_noisy_transparent_thumbnail_fits_both_caps() {
        let mut seed = 123_u32;
        let pixels = image::RgbaImage::from_fn(400, 300, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            image::Rgba(seed.to_be_bytes())
        });
        let mut raw = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels)
            .write_to(&mut raw, ImageFormat::Png)
            .unwrap();
        let n = normalize(raw.get_ref(), RAW_MAX_BYTES).unwrap();
        assert!(n.thumbnail_png.len() <= THUMB_MAX_BYTES);
        let thumb = image::load_from_memory(&n.thumbnail_png).unwrap();
        assert!(thumb.width() <= THUMB_MAX_SIDE && thumb.height() <= THUMB_MAX_SIDE);
        assert!(thumb.to_rgba8().pixels().any(|p| p.0[3] < 255));
    }

    #[test]
    fn sniff_requires_complete_magic_and_distinguishes_unsupported_images() {
        for (magic, kind) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), ImageType::Png),
            (b"\xff\xd8\xff", ImageType::Jpeg),
            (b"GIF87a", ImageType::Gif),
            (b"GIF89a", ImageType::Gif),
            (b"RIFFabcdWEBP", ImageType::Webp),
        ] {
            assert_eq!(sniff(magic), Some(kind));
            for len in 0..magic.len() {
                assert_eq!(sniff(&magic[..len]), None);
            }
        }
        assert_eq!(sniff(b"RIFFabcdWAVE"), None);
        assert!(matches!(normalize(b"BMdummy", 100), Err(ImageRefusal::UnsupportedType)));
    }

    #[test]
    fn extreme_aspect_ratios_and_portraits_never_exceed_bounds() {
        for (w, h) in [(1, 20000), (20000, 1), (2000, 3000), (20000, 2000), (1, 1)] {
            let (ow, oh) = fitted_size(w, h, OUT_MAX_SIDE, OUT_MAX_AREA);
            assert!(ow > 0 && oh > 0 && ow <= w && oh <= h);
            assert!(ow <= OUT_MAX_SIDE && oh <= OUT_MAX_SIDE);
            assert!(u64::from(ow) * u64::from(oh) <= OUT_MAX_AREA);
        }
    }
}
