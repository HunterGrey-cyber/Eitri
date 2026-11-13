use super::{normalize::check_dimensions, ImageRefusal};

fn take<'a>(rest: &mut &'a [u8], len: usize) -> Result<&'a [u8], ImageRefusal> {
    let data = rest.get(..len).ok_or(ImageRefusal::Undecodable)?;
    *rest = &rest[len..];
    Ok(data)
}

fn u16_le(bytes: &[u8]) -> u32 {
    u32::from(u16::from_le_bytes([bytes[0], bytes[1]]))
}

// The decoder reports the logical screen size but can allocate from a larger frame descriptor.
// Borrow slices through the first descriptor only; declared sizes must never drive an allocation.
pub(super) fn preflight(raw: &[u8]) -> Result<(), ImageRefusal> {
    let mut rest = raw;
    let header = take(&mut rest, 13)?;
    if &header[..6] != b"GIF87a" && &header[..6] != b"GIF89a" {
        return Err(ImageRefusal::Undecodable);
    }
    let screen = (u16_le(&header[6..8]), u16_le(&header[8..10]));
    check_dimensions(screen)?;
    if header[10] & 0x80 != 0 {
        let table_len = 3 * (1_usize << ((header[10] & 7) + 1));
        take(&mut rest, table_len)?;
    }
    loop {
        match take(&mut rest, 1)?[0] {
            0x21 => {
                take(&mut rest, 1)?; // Extension label; every extension uses a sub-block chain.
                loop {
                    let len = usize::from(take(&mut rest, 1)?[0]);
                    if len == 0 {
                        break;
                    }
                    take(&mut rest, len)?;
                }
            }
            0x2c => {
                let descriptor = take(&mut rest, 9)?;
                let left = u16_le(&descriptor[..2]);
                let top = u16_le(&descriptor[2..4]);
                let (width, height) = (u16_le(&descriptor[4..6]), u16_le(&descriptor[6..8]));
                check_dimensions((width, height))?;
                if left + width > screen.0 || top + height > screen.1 {
                    return Err(ImageRefusal::FrameOutsideScreen);
                }
                return Ok(());
            }
            _ => return Err(ImageRefusal::Undecodable),
        }
    }
}
