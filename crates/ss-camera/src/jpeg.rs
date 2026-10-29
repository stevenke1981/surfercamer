//! Minimal JPEG helpers (no decoding).

/// Width and height from the first SOFn marker, or `None` if not a JPEG.
pub fn dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    let _ = jpeg;
    todo!()
}
