//! Rendering synthetic display lists headlessly and checking pixels.
//!
//! Needs a GPU or a software adapter. Without one these tests are skipped
//! with a message, unless `N64RUSTY_REQUIRE_GPU` is set (CI sets it), in
//! which case they fail.

mod common;

use common::*;
use n64rusty_gfx::{Capture, Combiner, Frame, Image, Interpreter, Renderer};

fn renderer() -> Option<Renderer> {
    match Renderer::new() {
        Ok(r) => {
            eprintln!("adapter: {}", r.adapter_name());
            Some(r)
        }
        Err(e) if std::env::var_os("N64RUSTY_REQUIRE_GPU").is_none() => {
            eprintln!("skipping: {e}");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

fn frame(ram: &mut Ram, dl: &Dl) -> Frame {
    let addr = ram.dl(dl);
    let mut it = Interpreter::new();
    it.run(&ram.bytes, addr).expect("display list runs");
    it.take_frame()
}

fn near(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

#[track_caller]
fn assert_px(img: &Image, x: u32, y: u32, want: [u8; 4]) {
    let got = img.pixel(x, y);
    assert!(
        near(got, want),
        "pixel ({x}, {y}) is {got:?}, want {want:?}"
    );
}

/// Clear to blue with a fill rectangle, then the given body.
fn scene(ram: &mut Ram, body: impl FnOnce(&mut Ram, &mut Dl)) -> Dl {
    let mut dl = Dl::default();
    setup(ram, &mut dl);
    dl.other_modes(CYCLE_FILL, 0)
        .cimg(0x0E_0000, 320)
        .fill_color(0xFFFC_FFFC)
        .fill_rect(0, 0, 319, 239)
        .cimg(0x0F_0000, 320)
        .fill_color(0x003F_003F)
        .fill_rect(0, 0, 319, 239);
    body(ram, &mut dl);
    dl.end();
    dl
}

const BLUE: [u8; 4] = [0, 0, 255, 255];
const RED: [u8; 4] = [255, 0, 0, 255];

#[test]
fn fill_then_primitive_triangle() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let verts = ram.vertices(&[
        v(-50, -50, 0, [255; 4]),
        v(50, -50, 0, [255; 4]),
        v(0, 50, 0, [255; 4]),
    ]);
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1, 0)
            .combine(pass(PRIM))
            .prim(0xFF00_00FF)
            .vtx(verts, 3, 0)
            .tri(0, 1, 2);
    });
    let f = frame(&mut ram, &dl);
    for scale in [1, 2] {
        let img = r.render(&f, 320 * scale, 240 * scale).unwrap();
        // Object y is up: the apex (0, 50) is above the centre.
        assert_px(&img, 160 * scale, 130 * scale, RED);
        assert_px(&img, 160 * scale, 50 * scale, BLUE);
        assert_px(&img, 5, 5, BLUE);
    }
}

#[test]
fn shade_is_interpolated_and_combined() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let white = [255; 4];
    let verts = ram.vertices(&[
        v(-100, -100, 0, white),
        v(100, -100, 0, white),
        v(100, 100, 0, white),
        v(-100, 100, 0, [0, 0, 0, 255]),
    ]);
    // (SHADE - 0) * ENV + 0: white shade times a green environment.
    let modulate = Combiner::both([SHADE, 15, ENV, 7], [SHADE, 7, ENV, 7]).to_raw();
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1, 0)
            .geometry(0, G_SHADE | G_SHADING_SMOOTH)
            .combine(modulate)
            .env(0x00FF_00FF)
            .vtx(verts, 4, 0)
            .tri2([0, 1, 2], [0, 2, 3]);
    });
    let f = frame(&mut ram, &dl);
    let img = r.render(&f, 320, 240).unwrap();
    assert_px(&img, 315, 235, [0, 255, 0, 255]); // bottom right, all white
    let mid = img.pixel(10, 10); // near the black top-left corner
    assert!(mid[1] < 60 && mid[0] == 0, "{mid:?}");
}

#[test]
fn textured_triangles_sample_the_texture() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    // 2x2 RGBA16 checker, padded to one 64-bit line per row.
    let mut tex = Vec::new();
    for row in [[0xF801u16, 0x07C1], [0x07C1, 0xF801]] {
        for t in row {
            tex.extend_from_slice(&t.to_be_bytes());
        }
        tex.extend_from_slice(&[0; 4]);
    }
    let tex = ram.put(&tex);
    let mut verts = [
        v(-100, -100, 0, [255; 4]),
        v(100, -100, 0, [255; 4]),
        v(100, 100, 0, [255; 4]),
        v(-100, 100, 0, [255; 4]),
    ];
    // Top of the screen is t = 0.
    verts[0].st = [0, 2 << 5];
    verts[1].st = [2 << 5, 2 << 5];
    verts[2].st = [2 << 5, 0];
    verts[3].st = [0, 0];
    let verts = ram.vertices(&verts);
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1 | FILTER_POINT, 0)
            .combine(pass(TEXEL0))
            .texture(true, 0)
            .timg(0, 2, 1, tex)
            .tile(0, 2, 0, 0, 7)
            .load_block(7, 8)
            .tile(0, 2, 1, 0, 0)
            .tile_size(0, 2, 2)
            .vtx(verts, 4, 0)
            .tri2([0, 1, 2], [0, 2, 3]);
    });
    let f = frame(&mut ram, &dl);
    let img = r.render(&f, 320, 240).unwrap();
    let green = [0, 255, 0, 255];
    assert_px(&img, 80, 60, RED);
    assert_px(&img, 240, 60, green);
    assert_px(&img, 80, 180, green);
    assert_px(&img, 240, 180, RED);
}

#[test]
fn depth_buffer_keeps_the_nearer_triangle() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    // Two full-screen quads; the red one is nearer but drawn first.
    let quad = |z, c| {
        [
            v(-100, -100, z, c),
            v(100, -100, z, c),
            v(100, 100, z, c),
            v(-100, 100, z, c),
        ]
    };
    let mut both = quad(-50, [255, 0, 0, 255]).to_vec();
    both.extend(quad(50, [0, 255, 0, 255]));
    let verts = ram.vertices(&both);
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1, ZBUF)
            .geometry(0, G_ZBUFFER | G_SHADE | G_SHADING_SMOOTH)
            .combine(pass(SHADE))
            .vtx(verts, 8, 0)
            .tri2([0, 1, 2], [0, 2, 3])
            .tri2([4, 5, 6], [4, 6, 7]);
    });
    let f = frame(&mut ram, &dl);
    let img = r.render(&f, 320, 240).unwrap();
    assert_px(&img, 160, 120, RED);
}

#[test]
fn translucent_render_mode_blends_with_the_framebuffer() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let c = [255, 0, 0, 128];
    let verts = ram.vertices(&[
        v(-100, -100, 0, c),
        v(100, -100, 0, c),
        v(100, 100, 0, c),
        v(-100, 100, 0, c),
    ]);
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1, XLU)
            .geometry(0, G_SHADE | G_SHADING_SMOOTH)
            .combine(pass(SHADE))
            .vtx(verts, 4, 0)
            .tri2([0, 1, 2], [0, 2, 3]);
    });
    let f = frame(&mut ram, &dl);
    let img = r.render(&f, 320, 240).unwrap();
    let p = img.pixel(160, 120);
    assert!(
        (120..=136).contains(&p[0]) && (119..=135).contains(&p[2]),
        "{p:?}"
    );
}

#[test]
fn back_faces_are_culled() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    // Clockwise on screen: a back face.
    let verts = ram.vertices(&[
        v(-50, -50, 0, [255; 4]),
        v(0, 50, 0, [255; 4]),
        v(50, -50, 0, [255; 4]),
    ]);
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1, 0)
            .geometry(0, G_CULL_BACK)
            .combine(pass(PRIM))
            .prim(0xFF00_00FF)
            .vtx(verts, 3, 0)
            .tri(0, 1, 2);
    });
    let f = frame(&mut ram, &dl);
    let img = r.render(&f, 320, 240).unwrap();
    assert_px(&img, 160, 130, BLUE);
}

#[test]
fn captures_only_chosen_frames_to_png() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let dl = scene(&mut ram, |_, _| {});
    let f = frame(&mut ram, &dl);
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("gfx-capture");
    let _ = std::fs::remove_dir_all(&dir);
    let cap = Capture::new(&dir, [2]);
    assert_eq!(cap.frame(1, &mut r, &f).unwrap(), None);
    let path = cap.frame(2, &mut r, &f).unwrap().expect("frame 2 saved");
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (320, 240));
    assert_eq!(&buf[..4], &BLUE);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn scissor_limits_drawing() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let dl = scene(&mut ram, |_, dl| {
        dl.scissor(100, 100, 200, 150)
            .other_modes(CYCLE_FILL, 0)
            .fill_color(0xF801_F801)
            .fill_rect(0, 0, 319, 239);
    });
    let f = frame(&mut ram, &dl);
    for scale in [1, 2] {
        let img = r.render(&f, 320 * scale, 240 * scale).unwrap();
        assert_px(&img, 150 * scale, 125 * scale, RED);
        assert_px(&img, 50 * scale, 125 * scale, BLUE);
        assert_px(&img, 150 * scale, 200 * scale, BLUE);
    }
}

/// Renderers made and used on many threads at once, as parallel tests and
/// a runner with several outputs do. Adapter discovery used to run per
/// renderer and could unload a driver another thread was using.
#[test]
fn renderers_on_many_threads_at_once() {
    let Some(_) = renderer() else { return };
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..3 {
                    let mut ram = Ram::default();
                    let dl = scene(&mut ram, |_, _| {});
                    let f = frame(&mut ram, &dl);
                    let mut r = Renderer::new().unwrap();
                    let img = r.render(&f, 64, 48).unwrap();
                    assert_px(&img, 32, 24, BLUE);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn window_path_letterboxes_at_window_size() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let dl = scene(&mut ram, |_, _| {});
    let f = frame(&mut ram, &dl);
    // A wide window: 4:3 picture 640x480 in the middle, bars at the sides.
    assert_eq!(n64rusty_gfx::fit(800, 480, 320, 240), (80, 0, 640, 480));
    let img = r.render_letterboxed(&f, 800, 480).unwrap();
    assert_px(&img, 40, 240, [0, 0, 0, 255]);
    assert_px(&img, 760, 240, [0, 0, 0, 255]);
    assert_px(&img, 90, 10, BLUE);
    assert_px(&img, 710, 470, BLUE);
}

#[test]
fn window_path_draws_at_full_screen_sizes() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let dl = scene(&mut ram, |_, _| {});
    let f = frame(&mut ram, &dl);
    // A 4K full-screen window: wider and taller than the 2048 the lowest
    // GPU limits allow, which crashed going full screen.
    let img = r.render_letterboxed(&f, 3840, 2160).unwrap();
    assert_px(&img, 100, 1080, [0, 0, 0, 255]);
    assert_px(&img, 1920, 1080, BLUE);
}

#[test]
fn widescreen_shows_the_sides_and_keeps_shapes() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    // A triangle just past the right edge of the 4:3 view.
    let verts = ram.vertices(&[
        v(105, -20, 0, [255; 4]),
        v(130, -20, 0, [255; 4]),
        v(130, 20, 0, [255; 4]),
    ]);
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_1, 0)
            .combine(pass(PRIM))
            .prim(0xFF00_00FF)
            .vtx(verts, 3, 0)
            .tri(0, 1, 2)
            // A small centred rectangle, as a HUD element: 20 pixels wide.
            .other_modes(CYCLE_FILL, 0)
            .fill_color(0x07C1_07C1)
            .fill_rect(150, 110, 169, 129);
    });
    let f = frame(&mut ram, &dl);
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    // Off: black bars, the triangle is outside the picture.
    let img = r.render_letterboxed(&f, 640, 360).unwrap();
    assert_px(&img, 5, 180, BLACK);
    assert_px(&img, 615, 180, BLACK);

    r.set_widescreen(true);
    let img = r.render_letterboxed(&f, 640, 360).unwrap();
    // The full-width background reaches both edges.
    assert_px(&img, 2, 180, BLUE);
    assert_px(&img, 637, 180, BLUE);
    // The 4:3 picture keeps its shape in the middle (1.5 times larger),
    // so the triangle shows in the extra width at the side.
    assert_px(&img, 615, 180, RED);
    // The rectangle is 30 pixels wide, not stretched to 40.
    assert_px(&img, 320, 180, GREEN);
    assert_px(&img, 338, 180, BLUE);
    assert_px(&img, 302, 180, BLUE);

    // A 4:3 target is the same as without widescreen.
    let a = r.render(&f, 320, 240).unwrap();
    r.set_widescreen(false);
    assert_eq!(a.rgba, r.render(&f, 320, 240).unwrap().rgba);
}

/// A red square at object x offset `dx`, drawn from the same display list
/// code each frame, as a game redraws a moving object.
fn moved_square(dx: f32) -> Frame {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[
        v(-10, -10, 0, [255; 4]),
        v(10, -10, 0, [255; 4]),
        v(10, 10, 0, [255; 4]),
        v(-10, 10, 0, [255; 4]),
    ]);
    let mut m = n64rusty_gfx::IDENTITY;
    m[3][0] = dx;
    let mtx = ram.matrix(m);
    let dl = scene(&mut ram, |_, dl| {
        dl.mtx(mtx, LOAD)
            .other_modes(CYCLE_1, 0)
            .combine(pass(PRIM))
            .prim(0xFF00_00FF)
            .vtx(verts, 4, 0)
            .tri2([0, 1, 2], [0, 2, 3]);
    });
    frame(&mut ram, &dl)
}

#[test]
fn pictures_between_frames_blend_the_movement() {
    let Some(mut r) = renderer() else { return };
    // The square moves from x = -15 to x = +15 (pixel 136 to 184).
    let (prev, cur) = (moved_square(-15.0), moved_square(15.0));
    let at = |img: &Image, x| img.pixel(x, 120) == RED;
    let start = r.render_between(&prev, &cur, 0.0, 320, 240).unwrap();
    assert!(at(&start, 136) && !at(&start, 160) && !at(&start, 184));
    let half = r.render_between(&prev, &cur, 0.5, 320, 240).unwrap();
    assert!(!at(&half, 136) && at(&half, 160) && !at(&half, 184));
    let end = r.render_between(&prev, &cur, 1.0, 320, 240).unwrap();
    assert_eq!(end.rgba, r.render_letterboxed(&cur, 320, 240).unwrap().rgba);
    // Half the screen in one frame is a cut: shown where it ends up.
    let (far, cur) = (moved_square(-50.0), moved_square(50.0));
    let cut = r.render_between(&far, &cur, 0.5, 320, 240).unwrap();
    assert_eq!(cut.rgba, r.render_letterboxed(&cur, 320, 240).unwrap().rgba);
}

#[test]
fn widescreen_stretches_a_row_of_background_tiles() {
    let Some(mut r) = renderer() else { return };
    let mut ram = Ram::default();
    let dl = scene(&mut ram, |_, dl| {
        dl.other_modes(CYCLE_FILL, 0)
            // Two tiles edge to edge across the top band: a background.
            .fill_color(0x07C1_07C1)
            .fill_rect(0, 0, 159, 39)
            .fill_rect(160, 0, 319, 39)
            // Two pieces with a gap along the bottom, as a line of text.
            .fill_rect(0, 200, 99, 239)
            .fill_rect(220, 200, 319, 239)
            // A menu grid row edge to edge but inset from the frame edges.
            .fill_rect(25, 100, 159, 139)
            .fill_rect(160, 100, 294, 139);
    });
    let f = frame(&mut ram, &dl);
    r.set_widescreen(true);
    let img = r.render_letterboxed(&f, 640, 360).unwrap();
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    assert_px(&img, 2, 20, GREEN);
    assert_px(&img, 637, 20, GREEN);
    // The gapped row and the inset grid keep their 4:3 place.
    assert_px(&img, 2, 330, BLUE);
    assert_px(&img, 2, 180, BLUE);
    assert_px(&img, 120, 180, GREEN);
}
