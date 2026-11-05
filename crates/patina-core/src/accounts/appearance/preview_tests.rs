use super::*;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use std::io::Cursor;

fn encode(pixels: RgbaImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(pixels)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}
fn atlas(height: u32) -> RgbaImage {
    // Coordinates are the data, allowing hand-picked UV oracles independent of compositor tables.
    RgbaImage::from_fn(64, height, |x, y| Rgba([x as u8, y as u8, 200, 255]))
}
fn preview(model: SkinModel, side: PreviewSide) -> RgbaImage {
    let upload = SkinUpload::from_png(encode(atlas(64)), model).unwrap();
    compose_preview(&PreviewInput::new(&upload).with_side(side).with_outer(false)).unwrap()
}
#[test]
fn preview_front_back_uv() {
    let front = preview(SkinModel::Classic, PreviewSide::Front);
    assert_eq!(front.dimensions(), (24, 40));
    for (dst, src) in [
        ((8, 2), (8, 8)),
        ((8, 10), (20, 20)),
        ((4, 10), (44, 20)),
        ((16, 10), (36, 52)),
        ((8, 22), (4, 20)),
        ((12, 22), (20, 52)),
    ] {
        assert_eq!(front.get_pixel(dst.0, dst.1).0, [src.0, src.1, 200, 255]);
    }
    let back = preview(SkinModel::Classic, PreviewSide::Back);
    for (dst, src) in [
        ((8, 2), (24, 8)),
        ((8, 10), (32, 20)),
        ((16, 10), (52, 20)),
        ((4, 10), (44, 52)),
        ((12, 22), (12, 20)),
        ((8, 22), (28, 52)),
    ] {
        assert_eq!(back.get_pixel(dst.0, dst.1).0, [src.0, src.1, 200, 255]);
    }
}
#[test]
fn preview_classic_slim_center() {
    for side in [PreviewSide::Front, PreviewSide::Back] {
        let classic = preview(SkinModel::Classic, side);
        let slim = preview(SkinModel::Slim, side);
        assert_eq!(classic.dimensions(), slim.dimensions());
        assert_eq!(classic.get_pixel(4, 10)[3], 255);
        assert_eq!(slim.get_pixel(4, 10)[3], 0);
        assert_eq!(slim.get_pixel(5, 10)[3], 255);
        assert_eq!(slim.get_pixel(18, 10)[3], 255);
        assert_eq!(slim.get_pixel(19, 10)[3], 0);
        assert_eq!(classic.get_pixel(8, 2), slim.get_pixel(8, 2));
    }
    let back = preview(SkinModel::Slim, PreviewSide::Back);
    assert_eq!(back.get_pixel(16, 10).0, [51, 20, 200, 255]);
    assert_eq!(back.get_pixel(5, 10).0, [43, 52, 200, 255]);
}
#[test]
fn preview_legacy_mirrors_limbs() {
    let upload = SkinUpload::from_png(encode(atlas(32)), SkinModel::Classic).unwrap();
    let front = compose_preview(&PreviewInput::new(&upload).with_outer(false)).unwrap();
    assert_eq!(front.get_pixel(16, 10).0, [47, 20, 200, 255]);
    assert_eq!(front.get_pixel(19, 10).0, [44, 20, 200, 255]);
    assert_eq!(front.get_pixel(12, 22).0, [7, 20, 200, 255]);
    let back = compose_preview(
        &PreviewInput::new(&upload)
            .with_side(PreviewSide::Back)
            .with_outer(false),
    )
    .unwrap();
    assert_eq!(back.get_pixel(4, 10).0, [55, 20, 200, 255]);
    assert_eq!(back.get_pixel(8, 22).0, [15, 20, 200, 255]);
    assert!(upload.is_legacy());
    assert!(matches!(
        SkinUpload::from_png(encode(atlas(32)), SkinModel::Slim),
        Err(AppearanceError::LegacySlim)
    ));
}
#[test]
fn preview_outer_alpha() {
    let mut skin = RgbaImage::from_pixel(64, 64, Rgba([0, 0, 0, 0]));
    skin.put_pixel(8, 8, Rgba([200, 0, 0, 255]));
    skin.put_pixel(40, 8, Rgba([0, 0, 200, 128]));
    skin.put_pixel(20, 20, Rgba([80, 90, 100, 255]));
    let upload = SkinUpload::from_png(encode(skin), SkinModel::Classic).unwrap();
    let base = compose_preview(&PreviewInput::new(&upload).with_outer(false)).unwrap();
    let outer = compose_preview(&PreviewInput::new(&upload)).unwrap();
    assert_eq!(base.get_pixel(8, 2).0, [200, 0, 0, 255]);
    assert!((99..=100).contains(&outer.get_pixel(8, 2)[0]));
    assert!((100..=101).contains(&outer.get_pixel(8, 2)[2]));
    assert_eq!(outer.get_pixel(8, 10).0, [80, 90, 100, 255]);
}
#[test]
fn preview_back_cape_occlusion() {
    let upload = SkinUpload::from_png(encode(atlas(64)), SkinModel::Classic).unwrap();
    let mut cape = RgbaImage::from_pixel(64, 32, Rgba([0, 0, 0, 0]));
    cape.put_pixel(1, 1, Rgba([255, 0, 0, 255]));
    cape.put_pixel(2, 1, Rgba([0, 255, 0, 255]));
    cape.put_pixel(10, 16, Rgba([0, 0, 255, 255]));
    let cape = CapeTexture::from_png(&encode(cape)).unwrap();
    let input = PreviewInput::new(&upload).with_cape(cape).with_outer(false);
    let front = compose_preview(&input).unwrap();
    assert_eq!(front.get_pixel(8, 10).0, [20, 20, 200, 255]);
    let back = compose_preview(&input.with_side(PreviewSide::Back)).unwrap();
    assert_eq!(back.get_pixel(7, 10).0, [255, 0, 0, 255]);
    assert_eq!(back.get_pixel(8, 10).0, [0, 255, 0, 255]);
    assert_eq!(back.get_pixel(16, 25).0, [0, 0, 255, 255]);
    assert_eq!(back.get_pixel(9, 10).0, [33, 20, 200, 255]);
}
#[test]
fn appearance_png_validation_and_immutable_bytes() {
    let bytes = encode(atlas(64));
    let upload = SkinUpload::from_png(bytes.clone(), SkinModel::Slim).unwrap();
    assert_eq!(upload.bytes(), bytes);
    assert_eq!(upload.model(), SkinModel::Slim);
    for (w, h) in [(63, 64), (64, 33), (128, 128), (1, 1)] {
        assert!(SkinUpload::from_png(encode(RgbaImage::new(w, h)), SkinModel::Classic).is_err());
    }
    assert!(SkinUpload::from_png(b"not PNG".to_vec(), SkinModel::Classic).is_err());
    assert!(SkinUpload::from_png(bytes[..bytes.len() / 2].to_vec(), SkinModel::Classic).is_err());
    assert!(matches!(
        SkinUpload::from_png(vec![0; 256 * 1024 + 1], SkinModel::Classic),
        Err(AppearanceError::TooLarge)
    ));
    assert!(CapeTexture::from_png(&bytes).is_err());
    // Claimed ancillary size cannot overflow or trigger a metadata allocation.
    let mut malformed = bytes[..33].to_vec();
    malformed.extend_from_slice(&u32::MAX.to_be_bytes());
    malformed.extend_from_slice(b"iCCP");
    assert!(SkinUpload::from_png(malformed, SkinModel::Classic).is_err());
}

#[test]
fn appearance_cape_thumbnail_uses_bounded_outward_face() {
    let cape = CapeTexture::from_png(&encode(atlas(32))).unwrap();
    let thumb = cape.thumbnail();
    assert_eq!(thumb.dimensions(), (10, 16));
    assert_eq!(thumb.get_pixel(0, 0).0, [1, 1, 200, 255]);
    assert_eq!(thumb.get_pixel(9, 15).0, [10, 16, 200, 255]);
}

#[test]
fn appearance_preview_java_base_alpha_and_legacy_opaque_hat_semantics() {
    let mut pixels = RgbaImage::new(64, 64);
    pixels.put_pixel(8, 8, Rgba([80, 90, 100, 0]));
    let upload = SkinUpload::from_png(encode(pixels), SkinModel::Classic).unwrap();
    assert_eq!(
        compose_preview(&PreviewInput::new(&upload).with_outer(false))
            .unwrap()
            .get_pixel(8, 2)
            .0,
        [80, 90, 100, 255]
    );
    let bytes = encode(atlas(32));
    let upload = SkinUpload::from_png(bytes.clone(), SkinModel::Classic).unwrap();
    assert_eq!(
        compose_preview(&PreviewInput::new(&upload))
            .unwrap()
            .get_pixel(8, 2)
            .0,
        [8, 8, 200, 255],
        "unused opaque legacy hat region is not a hat"
    );
    assert_eq!(upload.bytes(), bytes);
}
