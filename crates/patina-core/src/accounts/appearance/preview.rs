use super::{AppearanceError, CapeTexture, SkinModel, SkinUpload};
use image::{Pixel, RgbaImage};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSide {
    #[default]
    Front,
    Back,
}

#[derive(Debug, Clone)]
pub struct PreviewInput {
    skin: SkinUpload,
    cape: Option<CapeTexture>,
    side: PreviewSide,
    outer: bool,
}
impl PreviewInput {
    pub fn new(skin: &SkinUpload) -> Self {
        Self {
            skin: skin.clone(),
            cape: None,
            side: PreviewSide::Front,
            outer: true,
        }
    }
    pub fn with_cape(mut self, cape: CapeTexture) -> Self {
        self.cape = Some(cape);
        self
    }
    pub fn with_side(mut self, side: PreviewSide) -> Self {
        self.side = side;
        self
    }
    pub fn with_outer(mut self, outer: bool) -> Self {
        self.outer = outer;
        self
    }
}

/// Fixed 24×40 transparent canvas, shared center/baseline for either model and side.
/// The only magnification belongs to presentation and must use nearest-neighbor sampling.
pub fn compose_preview(input: &PreviewInput) -> Result<RgbaImage, AppearanceError> {
    let mut out = RgbaImage::new(24, 40);
    let skin = &input.skin.pixels;
    let back = input.side == PreviewSide::Back;
    let legacy = input.skin.is_legacy();
    let arm = if input.skin.model == SkinModel::Slim {
        3
    } else {
        4
    };
    let arm_back = if arm == 3 { 7 } else { 8 };
    // UV rectangles are orthographic Minecraft Java faces, not projections of a 3D scene.
    let mut face = |uv, dst, size, mirror| blit(&mut out, skin, uv, dst, size, mirror, true);
    face((if back { 24 } else { 8 }, 8), (8, 2), (8, 8), false);
    face((if back { 32 } else { 20 }, 20), (8, 10), (8, 12), false);
    let (right_arm_x, left_arm_x, right_leg_x, left_leg_x) = if back {
        (16, 8 - arm, 12, 8)
    } else {
        (8 - arm, 16, 8, 12)
    };
    face(
        (44 + if back { arm_back } else { 0 }, 20),
        (right_arm_x, 10),
        (arm, 12),
        false,
    );
    face(
        (
            if legacy {
                44 + if back { arm_back } else { 0 }
            } else {
                36 + if back { arm_back } else { 0 }
            },
            if legacy { 20 } else { 52 },
        ),
        (left_arm_x, 10),
        (arm, 12),
        legacy,
    );
    face(
        (if back { 12 } else { 4 }, 20),
        (right_leg_x, 22),
        (4, 12),
        false,
    );
    face(
        (
            if legacy {
                if back { 12 } else { 4 }
            } else if back {
                28
            } else {
                20
            },
            if legacy { 20 } else { 52 },
        ),
        (left_leg_x, 22),
        (4, 12),
        legacy,
    );
    if input.outer {
        let mut face = |uv, dst, size, mirror| blit(&mut out, skin, uv, dst, size, mirror, false);
        // Java treats an entirely opaque legacy upper-right region as unused
        // background, not a hat. This is preview-only; original upload bytes stay intact.
        let legacy_hat =
            !legacy || (0..32).any(|y| (32..64).any(|x| skin.get_pixel(x, y)[3] < 128));
        if legacy_hat {
            face((if back { 56 } else { 40 }, 8), (8, 2), (8, 8), false);
        }
        if !legacy {
            face((if back { 32 } else { 20 }, 36), (8, 10), (8, 12), false);
            face(
                (44 + if back { arm_back } else { 0 }, 36),
                (right_arm_x, 10),
                (arm, 12),
                false,
            );
            face(
                (52 + if back { arm_back } else { 0 }, 52),
                (left_arm_x, 10),
                (arm, 12),
                false,
            );
            face(
                (if back { 12 } else { 4 }, 36),
                (right_leg_x, 22),
                (4, 12),
                false,
            );
            face(
                (if back { 12 } else { 4 }, 52),
                (left_leg_x, 22),
                (4, 12),
                false,
            );
        }
    }
    if back && let Some(cape) = &input.cape {
        // Cape's outward back face sits over the body, never on the front view.
        blit(&mut out, &cape.0, (1, 1), (7, 10), (10, 16), false, false);
    }
    Ok(out)
}
fn blit(
    out: &mut RgbaImage,
    atlas: &RgbaImage,
    uv: (u32, u32),
    dst: (u32, u32),
    size: (u32, u32),
    mirror: bool,
    opaque: bool,
) {
    for y in 0..size.1 {
        for x in 0..size.0 {
            let sx = if mirror { size.0 - 1 - x } else { x };
            let mut pixel = *atlas.get_pixel(uv.0 + sx, uv.1 + y);
            if opaque {
                pixel[3] = 255;
            }
            out.get_pixel_mut(dst.0 + x, dst.1 + y).blend(&pixel);
        }
    }
}
