//! Offscreen pixel regression for the production Play button, not native acceptance.
//! Run explicitly with a working WGPU/Metal adapter; never launches a game.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::ActiveTheme;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{HeadlessAppContext, Rgba};
use patina_app::core_bridge::testing::{self, FakeCatalog};
use patina_app::instances::RunState;
use patina_app::launch::{OpError, OpKind};
use patina_app::settings::{AppSettings, SettingsStore};
use patina_core::instance::InstanceStore;
use patina_core::paths::DataDirs;

fn luminance(rgb: Rgba) -> f32 {
    let linear = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
}

fn near(pixel: &image::Rgba<u8>, color: Rgba) -> bool {
    [color.r, color.g, color.b]
        .iter()
        .zip(pixel.0)
        .all(|(expected, actual)| (expected * 255. - f32::from(actual)).abs() <= 3.)
}

#[test]
#[ignore = "requires an offscreen graphics adapter; no native windows or games"]
fn disabled_play_has_neutral_surface_readable_label_and_rejects_activation() {
    let tmp = tempfile::tempdir().unwrap();
    let data = DataDirs::new(tmp.path());
    InstanceStore::new(data.instances())
        .create("Pack", "1.21.1")
        .unwrap();
    let rt = patina_app::runtime::start();
    let core = testing::core(rt.handle().clone(), data, Arc::new(FakeCatalog::new()));
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        Arc::new(patina_app::assets::AppAssets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.allow_parking();
    patina_app::i18n::set_language(patina_app::i18n::Language::En);
    let (handle, root) = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(core);
        patina_app::init(
            SettingsStore::new(tmp.path().into(), AppSettings::default()),
            cx,
        );
        patina_app::open_main_window(cx).unwrap()
    });
    let model = cx.update(|cx| root.read(cx).instances().read(cx).model().clone());
    for _ in 0..500 {
        cx.run_until_parked();
        if cx.update(|cx| model.read(cx).instances().iter().any(|i| i.id == "Pack")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (surface, foreground) = cx.update(|cx| {
        (
            Rgba::from(cx.theme().secondary),
            Rgba::from(cx.theme().foreground),
        )
    });
    let contrast = (luminance(foreground) + 0.05) / (luminance(surface) + 0.05);
    assert!(contrast >= 4.5, "label token contrast: {contrast}");
    // These are UI state fixtures, not processes. No launch backend is invoked.
    for state in [
        Some(RunState::Starting),
        Some(RunState::HeldElsewhere { pid: 123 }),
        None,
    ] {
        cx.update(|cx| {
            model.update(cx, |m, cx| {
                m.set_run_state("Pack", state.clone(), cx);
                if state.is_none() {
                    assert!(m.start_op(
                        "Pack",
                        OpKind::Reinstall,
                        |_| Box::pin(std::future::pending::<Result<(), OpError>>()),
                        |_, (), _| {},
                        None,
                        cx,
                    ));
                }
            })
        });
        cx.run_until_parked();
        let (bounds, scale) = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let play = window.find("launch");
                assert_eq!(play.disabled(), Some(true));
                assert_eq!(play.label(), Some("Play"));
                window.click("launch", cx);
                window.press("enter", cx);
                (play.bounds(), window.scale_factor())
            })
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                model
                    .read(cx)
                    .run_state("Pack", cx)
                    .map(std::mem::discriminant),
                state.as_ref().map(std::mem::discriminant),
            );
            assert_eq!(model.read(cx).is_op_running("Pack", cx), state.is_none());
        });
        let image = cx
            .capture_screenshot(handle)
            .expect("offscreen renderer required");
        let x = (f32::from(bounds.left()) * scale) as u32;
        let y = (f32::from(bounds.top()) * scale) as u32;
        let w = (f32::from(bounds.size.width) * scale) as u32;
        let h = (f32::from(bounds.size.height) * scale) as u32;
        let fill = image.get_pixel(x + 5, y + h / 2);
        assert!(
            near(fill, surface),
            "{state:?}: disabled surface is {fill:?}, expected {surface:?}"
        );
        // The right half contains the label, not the left-hand Play icon/spinner.
        let label_pixels = (y + 5..y + h - 5)
            .flat_map(|y| (x + w / 2..x + w - 10).map(move |x| (x, y)))
            .filter(|&(x, y)| near(image.get_pixel(x, y), foreground))
            .count();
        assert!(
            label_pixels >= 12,
            "{state:?}: readable label pixels: {label_pixels}"
        );
        println!("{state:?}: fill={fill:?}, label pixels={label_pixels}, contrast={contrast:.2}:1");
    }
}
