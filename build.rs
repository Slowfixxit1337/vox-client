use resvg::{tiny_skia, usvg};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/auth/auth-logo.svg");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let logo = include_bytes!("assets/auth/auth-logo.svg");
    let tree = usvg::Tree::from_data(logo, &usvg::Options::default()).expect("valid Vox logo");
    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let mut directory = vec![0, 0, 1, 0];
    directory.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut frames = Vec::new();
    let mut offset = (6 + sizes.len() * 16) as u32;
    for size in sizes {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("allocate icon frame");
        let scale = tiny_skia::Transform::from_scale(
            size as f32 / tree.size().width(),
            size as f32 / tree.size().height(),
        );
        resvg::render(&tree, scale, &mut pixmap.as_mut());
        let png = pixmap.encode_png().expect("encode icon frame");
        // ICO uses zero for a 256-pixel dimension.
        directory.extend_from_slice(&[size as u8, size as u8, 0, 0, 1, 0, 32, 0]);
        directory.extend_from_slice(&(png.len() as u32).to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
        frames.extend_from_slice(&png);
    }
    directory.extend_from_slice(&frames);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let icon = out.join("vox.ico");
    fs::write(&icon, directory).expect("write Vox icon");
    let rc = out.join("vox.rc");
    // GPUI loads the application icon from resource ID 1.
    fs::write(
        &rc,
        format!(
            "1 ICON \"{}\"\n",
            icon.display().to_string().replace('\\', "/")
        ),
    )
    .expect("write Vox resource script");
    embed_resource::compile_for(rc, ["vox-client"], embed_resource::NONE)
        .manifest_required()
        .expect("embed Vox icon");
}
