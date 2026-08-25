fn main() {
    ensure_icon();
    tauri_build::build()
}

fn ensure_icon() {
    let icon_dir = std::path::Path::new("icons");
    let icon_path = icon_dir.join("icon.png");
    if icon_path.exists() {
        return;
    }

    std::fs::create_dir_all(icon_dir).expect("create icon directory");
    let file = std::fs::File::create(icon_path).expect("create application icon");
    let mut encoder = png::Encoder::new(file, 32, 32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write icon header");

    let mut pixels = vec![0_u8; 32 * 32 * 4];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[36, 107, 58, 255]);
    }
    writer.write_image_data(&pixels).expect("write icon pixels");
}
