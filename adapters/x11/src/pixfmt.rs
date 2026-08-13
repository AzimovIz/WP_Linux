//! Introspects the X server's default visual/pixel format once at
//! startup, and converts render-server's RGBA8 frames into whatever byte
//! layout that visual actually wants. Unlike a Wayland/DRM buffer, X11
//! has no single fixed pixel format -- it depends on the server's own
//! `image_byte_order` and the chosen visual's channel masks, both read
//! directly here instead of assuming the common (but not universal)
//! little-endian BGRX8888 layout every real desktop happens to use today.

use x11rb::protocol::xproto::{ImageOrder, Screen, Setup};

#[derive(Debug, Clone, Copy)]
pub struct NativeFormat {
    pub depth: u8,
    msb_first: bool,
    red_shift: u32,
    green_shift: u32,
    blue_shift: u32,
}

/// Finds the root visual's `Visualtype` (for its channel masks) and the
/// matching pixmap `Format` (for `bits_per_pixel`) in the connection's
/// `Setup`. Fails with a message rather than guessing if the server
/// doesn't describe an 8-bit-per-channel TrueColor visual at 24 or 32
/// bits per pixel -- by far the universal case on real hardware, but not
/// one this should silently get wrong on whatever rare setup differs.
pub fn detect(setup: &Setup, screen: &Screen) -> Result<NativeFormat, String> {
    let visual = screen
        .allowed_depths
        .iter()
        .find_map(|depth| {
            depth
                .visuals
                .iter()
                .find(|v| v.visual_id == screen.root_visual)
        })
        .ok_or("root visual not found in Setup's allowed_depths")?;

    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .ok_or_else(|| format!("no pixmap format advertised for depth {}", screen.root_depth))?;

    if format.bits_per_pixel != 24 && format.bits_per_pixel != 32 {
        return Err(format!(
            "unsupported bits_per_pixel {} for the root visual (only 24/32 are supported)",
            format.bits_per_pixel
        ));
    }

    let red_shift = mask_shift(visual.red_mask).ok_or("red_mask is not a contiguous 8-bit field")?;
    let green_shift =
        mask_shift(visual.green_mask).ok_or("green_mask is not a contiguous 8-bit field")?;
    let blue_shift =
        mask_shift(visual.blue_mask).ok_or("blue_mask is not a contiguous 8-bit field")?;

    Ok(NativeFormat {
        depth: screen.root_depth,
        msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
        red_shift,
        green_shift,
        blue_shift,
    })
}

/// `mask` must be exactly `0xFF << n` for some `n` -- i.e. a plain
/// contiguous 8-bit channel, which every real TrueColor visual uses
/// (`n` doesn't have to be a multiple of 8 itself, just the mask needs
/// to be 8 one-bits with no gaps). Returns that `n`, or `None` for
/// anything else (a wider/narrower channel, or non-contiguous bits).
/// Clamped to 24 before use so an all-zero `mask` (`trailing_zeros() ==
/// 32`) can't shift `0xFFu32` by its own bit width and panic -- the
/// clamped shift still safely fails the equality check below.
fn mask_shift(mask: u32) -> Option<u32> {
    let shift = mask.trailing_zeros().min(24);
    (mask == 0xFFu32 << shift).then_some(shift)
}

/// Converts a tightly-packed RGBA8 buffer (render-server's own decoded
/// frame -- see `crates/render-server`'s PNG/BMP paths, both of which
/// this adapter decodes back into RGBA8 via the `image` crate before
/// calling this) into the server's native per-pixel byte layout, packed
/// 4 bytes/pixel (`ZPixmap` at depth 24 is still stored 4-byte-aligned
/// per pixel, same as depth 32 -- only the unused top byte's meaning
/// differs, which nothing here reads back anyway since the wallpaper
/// window is always fully opaque).
pub fn convert_rgba(fmt: &NativeFormat, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let (r, g, b) = (u32::from(px[0]), u32::from(px[1]), u32::from(px[2]));
        let word = (r << fmt.red_shift) | (g << fmt.green_shift) | (b << fmt.blue_shift);
        if fmt.msb_first {
            out.extend_from_slice(&word.to_be_bytes());
        } else {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(msb_first: bool, red_shift: u32, green_shift: u32, blue_shift: u32) -> NativeFormat {
        NativeFormat {
            depth: 24,
            msb_first,
            red_shift,
            green_shift,
            blue_shift,
        }
    }

    #[test]
    fn common_little_endian_bgrx_layout() {
        // The near-universal case on real x86 desktops: red_mask =
        // 0xFF0000, green_mask = 0xFF00, blue_mask = 0xFF, LSBFirst --
        // i.e. bytes on the wire come out B, G, R, X.
        let native = fmt(false, 16, 8, 0);
        let out = convert_rgba(&native, &[10, 20, 30, 255]);
        assert_eq!(out, vec![30, 20, 10, 0]);
    }

    #[test]
    fn mask_shift_rejects_non_byte_aligned_masks() {
        assert_eq!(mask_shift(0xFF0000), Some(16));
        assert_eq!(mask_shift(0x00FF00), Some(8));
        assert_eq!(mask_shift(0x0000FF), Some(0));
        assert_eq!(mask_shift(0x0F0F00), None); // non-contiguous bits
        assert_eq!(mask_shift(0xF0F000), None); // non-contiguous bits
    }
}
