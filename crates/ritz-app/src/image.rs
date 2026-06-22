//! Tiny PNG → egui texture helper, shared by the manager and the splash.

/// Decode a PNG and upload it as a texture, box-downsampling toward `max_dim`
/// first (premultiplied, so transparent edges don't fringe) so minification is
/// antialiased — egui's plain bilinear filter undersamples a large downscale.
/// Returns `None` if decoding fails.
pub fn load_logo_texture(
    ctx: &egui::Context,
    bytes: &[u8],
    max_dim: usize,
    name: &str,
) -> Option<egui::TextureHandle> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (mut w, mut h) = (info.width as usize, info.height as usize);
    let mut rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };

    // Halve (premultiplied box filter) until near the target resolution.
    while w > max_dim && h > max_dim && w % 2 == 0 && h % 2 == 0 {
        let (nw, nh) = (w / 2, h / 2);
        let mut out = vec![0u8; nw * nh * 4];
        for y in 0..nh {
            for x in 0..nw {
                let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let i = (((2 * y + dy) * w) + (2 * x + dx)) * 4;
                    let pa = rgba[i + 3] as u32;
                    r += rgba[i] as u32 * pa;
                    g += rgba[i + 1] as u32 * pa;
                    b += rgba[i + 2] as u32 * pa;
                    a += pa;
                }
                let o = (y * nw + x) * 4;
                out[o + 3] = (a / 4) as u8;
                if a > 0 {
                    out[o] = (r / a) as u8;
                    out[o + 1] = (g / a) as u8;
                    out[o + 2] = (b / a) as u8;
                }
            }
        }
        rgba = out;
        w = nw;
        h = nh;
    }

    let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR))
}
