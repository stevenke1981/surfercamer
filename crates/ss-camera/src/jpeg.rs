//! Minimal JPEG helpers (no decoding).

/// Marker bytes that follow `0xFF` for the Start Of Frame segments SOF0..SOF15,
/// excluding DHT (C4), JPG (C8) and DAC (CC) which share the range.
fn is_sof(marker: u8) -> bool {
    (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
}

/// Width and height from the first SOFn marker, or `None` if not a JPEG.
pub fn dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    loop {
        // A marker is 0xFF followed by its code; 0xFF fill bytes may repeat.
        if *jpeg.get(i)? != 0xFF {
            return None;
        }
        let mut j = i;
        while *jpeg.get(j)? == 0xFF {
            j += 1;
        }
        let marker = jpeg[j];
        i = j + 1;
        match marker {
            // Standalone markers without a length field.
            0x00 | 0x01 | 0xD0..=0xD7 | 0xD8 => continue,
            // End of image or start of scan before any frame header.
            0xD9 | 0xDA => return None,
            _ => {}
        }
        // `i` now points at the 2-byte segment length.
        let len = u16::from_be_bytes([*jpeg.get(i)?, *jpeg.get(i + 1)?]) as usize;
        if len < 2 {
            return None;
        }
        if is_sof(marker) {
            // length(2) precision(1) height(2) width(2)
            if len < 7 {
                return None;
            }
            let height = u16::from_be_bytes([*jpeg.get(i + 3)?, *jpeg.get(i + 4)?]);
            let width = u16::from_be_bytes([*jpeg.get(i + 5)?, *jpeg.get(i + 6)?]);
            return Some((u32::from(width), u32::from(height)));
        }
        i += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_of_encoded_jpeg() {
        let img = image::RgbImage::from_pixel(37, 21, image::Rgb([10, 200, 30]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
            .encode(img.as_raw(), 37, 21, image::ExtendedColorType::Rgb8)
            .expect("encode");
        assert_eq!(dimensions(&out), Some((37, 21)));
    }

    #[test]
    fn rejects_non_jpeg() {
        assert_eq!(dimensions(b""), None);
        assert_eq!(dimensions(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(dimensions(&[0xFF, 0xD8, 0xFF]), None);
    }

    #[test]
    fn skips_dht_and_app_segments() {
        // SOI, APP0 (len 4), DHT (C4, len 4 - must be skipped), SOF0 1x2 (h=2,w=1)
        let data = [
            0xFF, 0xD8, //
            0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, //
            0xFF, 0xC4, 0x00, 0x04, 0x00, 0x00, //
            0xFF, 0xC0, 0x00, 0x08, 0x08, 0x00, 0x02, 0x00, 0x01, 0x01,
        ];
        assert_eq!(dimensions(&data), Some((1, 2)));
    }

    #[test]
    fn stops_at_sos_without_sof() {
        let data = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02];
        assert_eq!(dimensions(&data), None);
    }
}
