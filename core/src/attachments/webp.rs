use super::{normalize::check_dimensions, ImageRefusal, DECODE_MAX_ALLOC};

type Size = (u32, u32);
type Chunk<'a> = (&'a [u8], &'a [u8]);

// Borrow bounded slices only: neither a RIFF length nor a bitstream dimension may drive an allocation.
fn next_chunk<'a>(rest: &mut &'a [u8]) -> Result<Option<Chunk<'a>>, ImageRefusal> {
    if rest.is_empty() {
        return Ok(None);
    }
    let header = rest.get(..8).ok_or(ImageRefusal::Undecodable)?;
    let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let end = 8_usize.checked_add(len).ok_or(ImageRefusal::Undecodable)?;
    let padded_end = end.checked_add(len & 1).ok_or(ImageRefusal::Undecodable)?;
    let data = rest.get(8..end).ok_or(ImageRefusal::Undecodable)?;
    *rest = rest.get(padded_end..).ok_or(ImageRefusal::Undecodable)?;
    Ok(Some((&header[..4], data)))
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

fn bitstream_size(tag: &[u8], data: &[u8]) -> Result<Size, ImageRefusal> {
    let size = match tag {
        b"VP8 " => {
            let h = data.get(..10).ok_or(ImageRefusal::Undecodable)?;
            if h[0] & 1 != 0 || h[3..6] != [0x9d, 1, 0x2a] {
                return Err(ImageRefusal::Undecodable);
            }
            (
                u32::from(u16::from_le_bytes([h[6], h[7]]) & 0x3fff),
                u32::from(u16::from_le_bytes([h[8], h[9]]) & 0x3fff),
            )
        }
        b"VP8L" => {
            let h = data.get(..5).ok_or(ImageRefusal::Undecodable)?;
            let bits = u32::from_le_bytes(h[1..5].try_into().unwrap());
            if h[0] != 0x2f || bits >> 29 != 0 {
                return Err(ImageRefusal::Undecodable);
            }
            ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
        }
        _ => return Err(ImageRefusal::Undecodable),
    };
    check_dimensions(size)?;
    Ok(size)
}

fn match_size(size: Size, canvas: Size, expected: Size) -> Result<(), ImageRefusal> {
    check_dimensions(size)?;
    if size.0 > canvas.0 || size.1 > canvas.1 {
        return Err(ImageRefusal::TooManyPixels {
            width: size.0,
            height: size.1,
        });
    }
    if size != expected {
        return Err(ImageRefusal::Undecodable);
    }
    Ok(())
}

fn check_frame(data: &[u8], canvas: Size) -> Result<(), ImageRefusal> {
    let h = data.get(..16).ok_or(ImageRefusal::Undecodable)?;
    let size = (u24(&h[6..9]) + 1, u24(&h[9..12]) + 1);
    match_size(size, canvas, size)?;
    if u24(&h[..3]) * 2 + size.0 > canvas.0 || u24(&h[3..6]) * 2 + size.1 > canvas.1 || h[15] & !3 != 0 {
        return Err(ImageRefusal::Undecodable);
    }
    let mut rest = &data[16..];
    let mut alpha = false;
    let mut image = false;
    while let Some((tag, data)) = next_chunk(&mut rest)? {
        match tag {
            b"ALPH" if !alpha && !image && !data.is_empty() => alpha = true,
            b"VP8 " | b"VP8L" => {
                match_size(bitstream_size(tag, data)?, canvas, size)?;
                if image || (alpha && tag == b"VP8L") {
                    return Err(ImageRefusal::Undecodable);
                }
                image = true;
            }
            _ => return Err(ImageRefusal::Undecodable),
        }
    }
    if !image {
        return Err(ImageRefusal::Undecodable);
    }
    Ok(())
}

/// Check every bitstream before constructing a decoder. Some WebP decode buffers ignore allocation
/// limits and use the embedded dimensions before comparing them to the canvas or alpha plane.
pub(super) fn preflight(raw: &[u8]) -> Result<Size, ImageRefusal> {
    let riff = raw.get(..12).ok_or(ImageRefusal::Undecodable)?;
    let len = u32::from_le_bytes(riff[4..8].try_into().unwrap()) as u64 + 8;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WEBP" || len != raw.len() as u64 {
        return Err(ImageRefusal::Undecodable);
    }
    let mut rest = &raw[12..];
    let (first, data) = next_chunk(&mut rest)?.ok_or(ImageRefusal::Undecodable)?;
    let extended = first == b"VP8X";
    let (canvas, animated) = if extended {
        if data.len() != 10 {
            return Err(ImageRefusal::Undecodable);
        }
        let size = (u24(&data[4..7]) + 1, u24(&data[7..10]) + 1);
        check_dimensions(size)?;
        if data[0] & 0xc1 != 0 || data[1..4] != [0; 3] {
            return Err(ImageRefusal::Undecodable);
        }
        (size, data[0] & 2 != 0)
    } else {
        (bitstream_size(first, data)?, false)
    };
    // image-webp keeps output, an RGBA frame and the compositing canvas alive together for
    // animation: reserve 3 * 4 bytes per canvas pixel. Stills need output plus RGBA scratch
    // (2 * 4); extended alpha also retains color planes during alpha decoding, so use 3 * 4.
    // Its decoder does not enforce image::Limits::max_alloc for these buffers.
    let buffers = if animated || (extended && data[0] & 0x10 != 0) {
        3
    } else {
        2
    };
    if u64::from(canvas.0) * u64::from(canvas.1) * 4 * buffers > DECODE_MAX_ALLOC {
        return Err(ImageRefusal::CanvasBuffersTooLarge);
    }
    let mut image = !extended;
    let mut alpha = false;
    let mut animation_header = false;
    let mut frames = false;
    while let Some((tag, data)) = next_chunk(&mut rest)? {
        match tag {
            b"VP8 " | b"VP8L" => {
                match_size(bitstream_size(tag, data)?, canvas, canvas)?;
                if image || animated || (alpha && tag == b"VP8L") {
                    return Err(ImageRefusal::Undecodable);
                }
                image = true;
            }
            b"VP8X" => return Err(ImageRefusal::Undecodable),
            b"ALPH" => {
                if !extended || animated || alpha || image || data.is_empty() {
                    return Err(ImageRefusal::Undecodable);
                }
                alpha = true;
            }
            b"ANIM" => {
                if !animated || animation_header || frames || data.len() != 6 {
                    return Err(ImageRefusal::Undecodable);
                }
                animation_header = true;
            }
            b"ANMF" => {
                if !animated || !animation_header {
                    return Err(ImageRefusal::Undecodable);
                }
                check_frame(data, canvas)?;
                frames = true;
            }
            _ => {}
        }
    }
    if (animated && !frames) || (!animated && !image) {
        return Err(ImageRefusal::Undecodable);
    }
    Ok(canvas)
}
