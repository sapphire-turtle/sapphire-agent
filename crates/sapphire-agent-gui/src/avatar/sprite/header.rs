//! Image headers: the pixel size of a PNG or WebP, read without decoding
//! it. The panel checks an upload against the canvas with it, and the
//! loader takes each image's size from its cached file, so the decoded
//! pixels need not stay in the main world.

use std::io::Read;
use std::path::Path;

use bevy::math::UVec2;

/// The most header bytes [`image_info`] needs: VP8 and VP8X end at 30.
const HEADER_LEN: u64 = 32;

/// The pixel size and file extension of a PNG or WebP, read from its
/// header only.
pub(crate) fn image_info(bytes: &[u8]) -> Result<(UVec2, &'static str), String> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let short = || "the image file is truncated".to_owned();
    let le24 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
    let (size, ext) = if bytes.starts_with(PNG) {
        let ihdr = bytes.get(8..24).ok_or_else(short)?;
        if &ihdr[4..8] != b"IHDR" {
            return Err("the PNG has no header".to_owned());
        }
        let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        (UVec2::new(be(&ihdr[8..12]), be(&ihdr[12..16])), "png")
    } else if bytes.len() >= 16 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        // Each variant needs only its own header bytes, counted from the
        // chunk's start at 12.
        let chunk = &bytes[12..];
        let need = |n: usize| chunk.get(..n).ok_or_else(short);
        let size = match &chunk[..4] {
            // Lossy: frame tag, start code, then 14-bit width and height.
            b"VP8 " => {
                let c = need(18)?;
                if c[11..14] != [0x9d, 0x01, 0x2a] {
                    return Err("the WebP has no VP8 start code".to_owned());
                }
                UVec2::new(
                    u32::from(u16::from_le_bytes([c[14], c[15]]) & 0x3fff),
                    u32::from(u16::from_le_bytes([c[16], c[17]]) & 0x3fff),
                )
            }
            // Lossless: signature byte, then 14-bit width-1 and height-1.
            b"VP8L" => {
                let c = need(13)?;
                if c[8] != 0x2f {
                    return Err("the WebP has no VP8L signature".to_owned());
                }
                let bits = u32::from_le_bytes([c[9], c[10], c[11], c[12]]);
                UVec2::new((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            // Extended: 24-bit width-1 and height-1 after the flags.
            b"VP8X" => {
                let c = need(18)?;
                UVec2::new(le24(&c[12..15]) + 1, le24(&c[15..18]) + 1)
            }
            _ => return Err("this WebP is not one the header can be read from".to_owned()),
        };
        (size, "webp")
    } else {
        return Err("the file is not a PNG or WebP image".to_owned());
    };
    if size.x == 0 || size.y == 0 {
        return Err(format!("the image is {}×{}", size.x, size.y));
    }
    Ok((size, ext))
}

/// The pixel size of the PNG or WebP in `file`, from its header only.
pub(crate) fn file_size(file: &Path) -> Result<UVec2, String> {
    let mut head = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(HEADER_LEN).read_to_end(&mut head))
        .map_err(|e| format!("cannot read it: {e}"))?;
    image_info(&head).map(|(size, _)| size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_info_reads_png_and_webp_headers() {
        let png = crate::avatar::sprite::load::tests::png(3, 5);
        assert_eq!(image_info(&png), Ok((UVec2::new(3, 5), "png")));

        let riff = |chunk: &[u8]| {
            let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
            b.extend_from_slice(chunk);
            b.resize(40, 0);
            b
        };
        // VP8X: flags, then 24-bit width-1 and height-1.
        let vp8x = riff(&[
            b'V', b'P', b'8', b'X', 10, 0, 0, 0, 0, 0, 0, 0, 99, 0, 0, 199, 0, 0,
        ]);
        assert_eq!(image_info(&vp8x), Ok((UVec2::new(100, 200), "webp")));
        // VP8L: signature 0x2f, then 14-bit width-1 and height-1.
        let packed: u32 = 49 | (79 << 14);
        let mut l = vec![b'V', b'P', b'8', b'L', 5, 0, 0, 0, 0x2f];
        l.extend_from_slice(&packed.to_le_bytes());
        assert_eq!(image_info(&riff(&l)), Ok((UVec2::new(50, 80), "webp")));
        // VP8: frame tag, start code, 14-bit width and height.
        let vp8 = riff(&[
            b'V', b'P', b'8', b' ', 10, 0, 0, 0, 0, 0, 0, 0x9d, 0x01, 0x2a, 30, 0, 40, 0,
        ]);
        assert_eq!(image_info(&vp8), Ok((UVec2::new(30, 40), "webp")));

        // Only what each variant needs: the shortest VP8L header.
        let short_l = [
            &b"RIFF\0\0\0\0WEBPVP8L\x05\0\0\0\x2f"[..],
            &packed.to_le_bytes(),
        ]
        .concat();
        assert_eq!(short_l.len(), 25);
        assert_eq!(image_info(&short_l), Ok((UVec2::new(50, 80), "webp")));
        // Truncated variants are errors, not panics.
        for full in [&vp8x, &riff(&l), &vp8] {
            let end = if std::ptr::eq(full, &vp8x) { 29 } else { 24 };
            assert!(image_info(&full[..end]).is_err(), "{end}");
        }
        assert!(image_info(&short_l[..24]).is_err());
        assert!(image_info(b"RIFF\0\0\0\0WEBPVP8").is_err());

        assert!(image_info(b"GIF89a....").is_err());
        assert!(image_info(&png[..10]).is_err());
    }

    #[test]
    fn zero_sized_images_are_refused() {
        let mut png = crate::avatar::sprite::load::tests::png(3, 5);
        png[16..20].copy_from_slice(&0u32.to_be_bytes());
        assert!(image_info(&png).is_err());
        let mut png = crate::avatar::sprite::load::tests::png(3, 5);
        png[20..24].copy_from_slice(&0u32.to_be_bytes());
        assert!(image_info(&png).is_err());
    }

    #[test]
    fn file_size_reads_the_cached_files_header() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, crate::avatar::sprite::load::tests::png(3, 5)).unwrap();
        assert_eq!(file_size(&file), Ok(UVec2::new(3, 5)));
        std::fs::write(&file, b"not an image at all, but long enough").unwrap();
        assert!(file_size(&file).is_err());
        assert!(file_size(&dir.path().join("gone.png")).is_err());
    }
}
