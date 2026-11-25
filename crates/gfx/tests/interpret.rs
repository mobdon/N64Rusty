//! Display-list interpretation, checked on the CPU with synthetic lists.

mod common;

use common::*;
use n64rusty_gfx::{Command, Frame, GfxError, Interpreter, Wrap};

fn run(ram: &Ram, dl: u32) -> Frame {
    let mut it = Interpreter::new();
    it.run(&ram.bytes, dl).expect("display list runs");
    it.take_frame()
}

#[test]
fn records_a_triangle_with_its_state() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[
        v(-50, -50, 0, [255, 0, 0, 255]),
        v(50, -50, 0, [0, 255, 0, 255]),
        v(0, 50, 0, [0, 0, 255, 128]),
    ]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    let combine = pass(SHADE);
    dl.other_modes(CYCLE_1, ZBUF)
        .geometry(0, G_ZBUFFER | G_SHADE | G_SHADING_SMOOTH)
        .combine(combine)
        .prim(0x11223344)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);

    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    let draws: Vec<_> = f.draws().collect();
    assert_eq!(draws.len(), 1);
    let d = draws[0];
    assert_eq!((d.first, d.count), (0, 3));
    assert_eq!(d.state.combine, combine);
    assert_eq!(d.state.other_l, ZBUF);
    assert_eq!(
        d.state.prim,
        [0x11, 0x22, 0x33, 0x44].map(|c| c as f32 / 255.0)
    );
    assert_eq!(d.state.geometry, G_ZBUFFER | G_SHADE | G_SHADING_SMOOTH);
    assert_eq!(f.vertices[0].pos, [-50.0, -50.0, 0.0]);
    assert_eq!(f.vertices[2].color, [0.0, 0.0, 1.0, 128.0 / 255.0]);
    let t = &f.transforms[f.vertices[0].transform as usize];
    assert!((t.projection[0][0] - 0.01).abs() < 1e-4);
    assert_eq!(t.viewport.scale, [160.0, 120.0]);
    assert_eq!(f.width, 320);
    assert_eq!(f.color_image, 0x0F_0000);
}

#[test]
fn second_triangle_with_same_state_extends_the_draw() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 4]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.vtx(verts, 4, 0).tri2([0, 1, 2], [1, 2, 3]);
    dl.prim(0xFF0000FF).tri(0, 1, 3).end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    let draws: Vec<_> = f.draws().collect();
    assert_eq!(draws.len(), 2);
    assert_eq!(draws[0].count, 6);
    assert_eq!(draws[1].first, 6);
}

#[test]
fn matrix_stack_push_multiply_pop() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4])]);
    let mut a = n64rusty_gfx::IDENTITY;
    a[3] = [1.0, 2.0, 3.0, 1.0];
    let mut b = n64rusty_gfx::IDENTITY;
    b[0][0] = 2.0;
    b[3][0] = -0.5;
    let (ma, mb) = (ram.matrix(a), ram.matrix(b));
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.mtx(ma, LOAD)
        .mtx(mb, PUSH)
        .vtx(verts, 1, 0)
        .pop(1)
        .vtx(verts, 1, 0)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert_eq!(f.transforms.len(), 2);
    // b applied first, then a: rows of b times a.
    assert_eq!(f.transforms[0].modelview, n64rusty_gfx::mat_mul(&b, &a));
    assert_eq!(f.transforms[0].modelview[3], [0.5, 2.0, 3.0, 1.0]);
    assert_eq!(f.transforms[1].modelview, a);
    // Tags tell the two loads apart.
    assert_ne!(f.transforms[0].tag, f.transforms[1].tag);
}

#[test]
fn segments_calls_and_branches() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    // Leaf list: draws one triangle with vertices from segment 6.
    let mut leaf = Dl::default();
    leaf.vtx(0x0600_0000, 3, 0).tri(0, 1, 2).end();
    let leaf = ram.dl(&leaf);
    // Middle list: branches (no return) to the leaf through segment 4.
    let mut mid = Dl::default();
    mid.branch(0x0400_0000 | (leaf - 0x1000));
    let mid = ram.dl(&mid);
    let mut root = Dl::default();
    setup(&mut ram, &mut root);
    root.segment(6, verts)
        .segment(4, 0x1000)
        .call(mid)
        .call(leaf) // returns here after the branch chain ends
        .end();
    let addr = ram.dl(&root);
    let f = run(&ram, addr);
    assert_eq!(f.vertices.len(), 6);
}

#[test]
fn fill_rectangles_clear_depth_or_draw_the_fill_colour() {
    let mut ram = Ram::default();
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.other_modes(CYCLE_FILL, 0)
        .cimg(0x0E_0000, 320) // the depth image
        .fill_color(0xFFFC_FFFC)
        .fill_rect(0, 0, 319, 239)
        .cimg(0x0F_0000, 320)
        .fill_color(0x07C1_07C1) // RGBA5551 green, opaque
        .fill_rect(10, 20, 29, 39)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert_eq!(f.commands[0], Command::ClearDepth);
    let d = f.draws().next().unwrap();
    assert!(d.state.rect);
    assert_eq!(d.state.prim, [0.0, 1.0, 0.0, 1.0]);
    let xs: Vec<f32> = f.vertices[..6].iter().map(|v| v.pos[0]).collect();
    // Fill mode includes the right edge: 10..30.
    assert_eq!(xs.iter().cloned().fold(f32::MAX, f32::min), 10.0);
    assert_eq!(xs.iter().cloned().fold(f32::MIN, f32::max), 30.0);
}

#[test]
fn loads_and_decodes_an_rgba16_texture() {
    let mut ram = Ram::default();
    // 4x2 RGBA16: red, green, blue, white / then all transparent black.
    let texels: [u16; 8] = [0xF801, 0x07C1, 0x003F, 0xFFFF, 0, 0, 0, 0];
    let tex: Vec<u8> = texels.iter().flat_map(|t| t.to_be_bytes()).collect();
    let tex = ram.put(&tex);
    let mut verts = [v(0, 0, 0, [255; 4]); 3];
    verts[1].st = [4 << 5, 0]; // s = 4.0 texels
    verts[2].st = [0, 2 << 5];
    let verts = ram.vertices(&verts);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.texture(true, 0)
        .timg(0, 2, 1, tex)
        .tile(0, 2, 0, 0, 7)
        .load_block(7, 8)
        .tile(0, 2, 1, 0, 0) // 4 texels * 2 bytes = one 64-bit line
        .tile_size(0, 4, 2)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    let d = f.draws().next().unwrap();
    let t = &f.textures[d.state.textures[0].unwrap() as usize];
    assert_eq!((t.width, t.height), (4, 2));
    assert_eq!(&t.rgba[..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
    assert_eq!(&t.rgba[12..16], &[255, 255, 255, 255]);
    assert_eq!(&t.rgba[16..20], &[0, 0, 0, 0]);
    // No mask: the hardware cannot wrap, so the tile clamps.
    assert_eq!(t.wrap, [Wrap::Clamp; 2]);
    assert_eq!(f.vertices[1].uv[0], [1.0, 0.0]);
    assert_eq!(f.vertices[2].uv[0], [0.0, 1.0]);
}

#[test]
fn decodes_a_palette_texture() {
    let mut ram = Ram::default();
    let pal: Vec<u8> = [0x0001u16, 0xF801, 0x07C1]
        .iter()
        .flat_map(|c| c.to_be_bytes())
        .collect();
    let pal = ram.put(&pal);
    // CI8, 4x2, rows padded to one 64-bit line.
    let tex = ram.put(&[1, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.other_modes(2 << 14, 0) // palette entries are RGBA16
        .texture(true, 0)
        .timg(0, 2, 1, pal)
        .tile(0, 2, 0, 256, 7)
        .load_tlut(7, 3)
        .timg(2, 1, 1, tex)
        .tile(2, 1, 0, 0, 7)
        .load_block(7, 16)
        .tile(2, 1, 1, 0, 0)
        .tile_size(0, 4, 2)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    let t = &f.textures[0];
    assert_eq!(&t.rgba[..4], &[255, 0, 0, 255]);
    assert_eq!(&t.rgba[4..8], &[0, 255, 0, 255]);
    assert_eq!(&t.rgba[16..20], &[0, 0, 0, 255]);
}

#[test]
fn lights_vertices_from_normals() {
    let mut ram = Ram::default();
    // Normal pointing at +z, stored in the colour bytes; alpha 200.
    let verts = ram.vertices(&[V {
        pos: [0, 0, 0],
        st: [0, 0],
        rgba: [0, 0, 127, 200],
    }]);
    // Directional light towards +z, half red; ambient a little blue.
    let light = ram.put(&[128, 0, 0, 0, 128, 0, 0, 0, 0, 0, 127, 0, 0, 0, 0, 0]);
    let ambient = ram.put(&[0, 0, 51, 0, 0, 0, 51, 0]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.geometry(0, G_LIGHTING)
        .num_lights(1)
        .light(light, 1)
        .light(ambient, 2)
        .vtx(verts, 1, 0)
        .tri(0, 0, 0)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    let c = f.vertices[0].color;
    assert!((c[0] - 128.0 / 255.0).abs() < 1e-5, "{c:?}");
    assert_eq!(c[1], 0.0);
    assert!((c[2] - 0.2).abs() < 1e-5, "{c:?}");
    assert_eq!(c[3], 200.0 / 255.0);
}

#[test]
fn light_colour_command_changes_a_loaded_light() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[V {
        pos: [0, 0, 0],
        st: [0, 0],
        rgba: [0, 0, 127, 255],
    }]);
    let light = ram.put(&[128, 0, 0, 0, 128, 0, 0, 0, 0, 0, 127, 0, 0, 0, 0, 0]);
    let ambient = ram.put(&[0, 0, 0, 0, 0, 0, 0, 0]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.geometry(0, G_LIGHTING)
        .num_lights(1)
        .light(light, 1)
        .light(ambient, 2)
        // gSPLightColor: light 1 becomes full green (both colour words),
        // the ambient (light 2, offset 24) a little blue.
        .light_color(0, 0x00FF_0000)
        .light_color(4, 0x00FF_0000)
        .light_color(24, 0x0000_3300)
        .vtx(verts, 1, 0)
        .tri(0, 0, 0)
        .end();
    let addr = ram.dl(&dl);
    let c = run(&ram, addr).vertices[0].color;
    assert_eq!(c[0], 0.0, "{c:?}");
    assert_eq!(c[1], 1.0, "{c:?}");
    assert!((c[2] - 0.2).abs() < 1e-5, "{c:?}");
}

#[test]
fn culling_both_faces_drops_triangles() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.geometry(0, G_CULL_FRONT | G_CULL_BACK)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    assert_eq!(run(&ram, addr).vertices.len(), 0);
}

#[test]
fn bad_lists_are_errors_or_warnings() {
    let mut ram = Ram::default();
    let mut it = Interpreter::new();
    // Starts outside RAM.
    assert!(matches!(
        it.run(&ram.bytes, 0x00F0_0000),
        Err(GfxError::OutOfRange { .. })
    ));
    // Branches to itself forever.
    let at = ram.put(&[0; 8]);
    let mut dl = Dl::default();
    dl.branch(at);
    ram.bytes[at as usize..at as usize + 8].copy_from_slice(
        &dl.0
            .iter()
            .flat_map(|w| w.to_be_bytes())
            .collect::<Vec<_>>(),
    );
    assert!(matches!(
        it.run(&ram.bytes, at),
        Err(GfxError::TooLong { .. })
    ));
    // An unknown command is skipped with a warning.
    let mut dl = Dl::default();
    dl.0.extend([0x7700_0000, 0]);
    dl.end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    it.run(&ram.bytes, addr).unwrap();
    assert_eq!(it.take_frame().warnings.len(), 1);
}

#[test]
fn state_carries_over_between_tasks_but_frames_are_separate() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    let mut first = Dl::default();
    setup(&mut ram, &mut first);
    first.segment(6, verts).end();
    let first = ram.dl(&first);
    let mut second = Dl::default();
    second.vtx(0x0600_0000, 3, 0).tri(0, 1, 2).end();
    let second = ram.dl(&second);
    let mut it = Interpreter::new();
    it.run(&ram.bytes, first).unwrap();
    it.run(&ram.bytes, second).unwrap();
    assert_eq!(it.take_frame().vertices.len(), 3);
    assert_eq!(it.take_frame().vertices.len(), 0);
}

#[test]
fn texture_rectangle_maps_texels_to_pixels() {
    let mut ram = Ram::default();
    let tex = ram.put(&[0xFF; 32]); // 4x4 RGBA16 white
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.other_modes(CYCLE_1, 0)
        .timg(0, 2, 1, tex)
        .tile(0, 2, 0, 0, 7)
        .load_block(7, 16)
        .tile(0, 2, 1, 0, 0)
        .tile_size(0, 4, 4)
        // 8x8 pixels at half a texel per pixel, starting at texel (0, 0).
        .tex_rect(0, [10, 20, 18, 28], [0, 0], [1 << 9, 1 << 9])
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    let d = f.draws().next().unwrap();
    assert!(d.state.rect && d.state.textures[0].is_some());
    let corners: Vec<_> = f.vertices[..6].iter().map(|v| (v.pos, v.uv[0])).collect();
    assert!(
        corners.contains(&([10.0, 20.0, -1.0], [0.0, 0.0])),
        "{corners:?}"
    );
    assert!(
        corners.contains(&([18.0, 28.0, -1.0], [1.0, 1.0])),
        "{corners:?}"
    );
}

/// The depth field is 15 bits; games send 0xFFFF for "farthest".
#[test]
fn primitive_depth_stays_in_the_depth_range() {
    let mut ram = Ram::default();
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.other_modes(CYCLE_1, 0)
        .prim_depth(0xFFFF)
        .tex_rect(0, [10, 20, 18, 28], [0, 0], [1 << 10, 1 << 10])
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert_eq!(f.vertices.len(), 6);
    assert!(
        f.vertices.iter().all(|v| v.pos[2] == 1.0),
        "{:?}",
        f.vertices[0]
    );
}

#[test]
fn malformed_lengths_from_guest_data_do_not_panic() {
    let mut ram = Ram::default();
    let mut dl = Dl::default();
    // Other-mode writes with a length field of 0xFF (256 bits) and shifts
    // that would run past the word.
    dl.0.extend([0xE200_00FF, 0x1234_5678, 0xE3FF_00FF, 0xFFFF_FFFF]);
    dl.0.extend([0xE2FF_FF1F, 0xFFFF_FFFF]);
    // Pop far more matrices than were pushed.
    dl.0.extend([0xD838_0002, 0xFFFF_FFC0]);
    // A one-pixel-wide colour image still gives a usable height.
    dl.cimg(0x0F_0000, 1);
    dl.end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert_eq!(f.height(), 1);
}

#[test]
fn modify_vertex_overwrites_colour_texture_and_screen_position() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(10, 20, 30, [0; 4]); 3]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.vtx(verts, 3, 0)
        .modify_vtx(0x10, 0, 0xFF80_00FF)
        .modify_vtx(0x14, 1, (64 << 16) | 32) // s = 2.0, t = 1.0 texels
        .modify_vtx(0x18, 2, (40 << 16) | 100) // screen (10, 25)
        .modify_vtx(0x1C, 2, 0x7FFF << 16)
        .scissor(8, 16, 300, 200)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    assert_eq!(f.vertices[0].color, [1.0, 128.0 / 255.0, 0.0, 1.0]);
    assert_eq!(f.vertices[0].pos, [10.0, 20.0, 30.0]);
    let moved = f.vertices[2];
    assert_eq!(moved.pos, [10.0, 25.0, 1.0]);
    assert_ne!(moved.transform, f.vertices[0].transform);
    let d = f.draws().next().unwrap();
    assert_eq!(d.state.scissor, Some([8.0, 16.0, 300.0, 200.0]));
}

#[test]
fn insert_matrix_edits_the_combined_matrix() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    // Integer parts of elements 0 and 1 become 2 and 0; fractions stay.
    dl.0.extend([0xDB00_0000, 0x0002_0000]);
    dl.vtx(verts, 3, 0).tri(0, 1, 2).end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    let t = &f.transforms[f.vertices[0].transform as usize];
    assert!((t.modelview[0][0] - 2.01).abs() < 1e-3, "{:?}", t.modelview);
    assert_eq!(t.projection, n64rusty_gfx::IDENTITY);
}

/// Hands back one colour for every pixel, and notes what it was asked for.
struct Solid {
    asked: Vec<(u32, u32, u32)>,
}

impl n64rusty_gfx::Readback for Solid {
    fn read(&mut self, _: &Frame, image: u32, width: u32, height: u32) -> Option<Vec<u8>> {
        self.asked.push((image, width, height));
        Some([255, 0, 0, 255].repeat((width * height) as usize))
    }
}

#[test]
fn fills_and_copies_land_in_ram_and_drawn_images_are_read_back() {
    const CYCLE_COPY: u32 = 2 << 20;
    let mut ram = Ram::default();
    // 4x4 RGBA16 texture: every texel opaque green.
    let tex = ram.put(&[0x07, 0xC1].repeat(16));
    let small = ram.put(&[0; 8 * 6 * 2]); // an 8-wide 16-bit image
    let drawn = ram.put(&[0; 8 * 6 * 2]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.cimg(small, 8)
        .other_modes(CYCLE_FILL, 0)
        .fill_color(0xF801_F801) // red
        .fill_rect(0, 0, 7, 0)
        .other_modes(CYCLE_COPY, 0)
        .timg(0, 2, 1, tex)
        .tile(0, 2, 0, 0, 7)
        .load_block(7, 16)
        .tile(0, 2, 1, 0, 0)
        .tile_size(0, 4, 4)
        // Copy mode steps 4 texels per clock: 4 << 10 is one per pixel.
        .tex_rect(0, [0, 1, 3, 1], [0, 0], [4 << 10, 1 << 10])
        // A 1-cycle draw goes to the GPU; loading from that image reads it back.
        .cimg(drawn, 8)
        .other_modes(CYCLE_1, 0)
        .tex_rect(0, [0, 0, 8, 6], [0, 0], [1 << 10, 1 << 10])
        .timg(0, 2, 8, drawn)
        .load_block(7, 16)
        .end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    let mut rb = Solid { asked: Vec::new() };
    it.run_readback(&mut ram.bytes, addr, &mut rb).unwrap();
    let px = |img: u32, i: usize| {
        u16::from_be_bytes([
            ram.bytes[img as usize + 2 * i],
            ram.bytes[img as usize + 2 * i + 1],
        ])
    };
    assert_eq!(px(small, 0), 0xF801);
    assert_eq!(px(small, 7), 0xF801);
    assert_eq!(px(small, 8), 0x07C1, "copied texel");
    assert_eq!(px(small, 11), 0x07C1);
    assert_eq!(px(small, 12), 0);
    assert_eq!(rb.asked, [(drawn, 8, 6)]);
    assert_eq!(px(drawn, 47), 0xF801, "read back red");
}
