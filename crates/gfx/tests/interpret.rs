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

/// A 4x2 RGBA16 tile with vertex 1 at s = 4 texels, drawn in `modes`.
fn uv_of_far_vertex(modes: u32) -> [f32; 2] {
    let mut ram = Ram::default();
    let tex = ram.put(&[0; 16]);
    let mut verts = [v(0, 0, 0, [255; 4]); 3];
    verts[1].st = [4 << 5, 0];
    let verts = ram.vertices(&verts);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.other_modes(modes, 0)
        .texture(true, 0)
        .timg(0, 2, 1, tex)
        .tile(0, 2, 0, 0, 7)
        .load_block(7, 8)
        .tile(0, 2, 1, 0, 0)
        .tile_size(0, 4, 2)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    run(&ram, addr).vertices[1].uv[0]
}

#[test]
fn perspective_correction_off_halves_texture_coordinates() {
    // The sun billboard relies on this: its strips are drawn at s 0..63 over
    // 32 texel rows.
    assert_eq!(uv_of_far_vertex(CYCLE_1)[0], 1.0);
    assert_eq!(uv_of_far_vertex(NO_PERSP)[0], 0.5);
}

#[test]
fn bilinear_texel_centres_sit_on_whole_coordinates() {
    // s = 4 is texel 4's centre: 4.5 of the 4-texel tile's width on a GPU.
    assert_eq!(uv_of_far_vertex(CYCLE_1 | FILTER_BILERP)[0], 4.5 / 4.0);
}

#[test]
fn texture_generation_maps_normals_through_the_look_at() {
    let mut ram = Ram::default();
    // Normal along +x, stored in the colour bytes.
    let verts = ram.vertices(&[V {
        pos: [0, 0, 0],
        st: [0, 0],
        rgba: [127, 0, 0, 255],
    }]);
    let x = ram.put(&[0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 0, 0, 0, 0, 0]);
    let y = ram.put(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 0, 0, 0, 0]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    // Scale 0x100: coordinates span 0..4 texels; the tile is 4x2.
    dl.geometry(0, G_LIGHTING | 0x4_0000)
        .look_at(x, 0)
        .look_at(y, 1)
        .texture_scaled(0, 0x100, 0x100)
        .tile(0, 2, 1, 0, 0)
        .tile_size(0, 4, 2)
        .vtx(verts, 1, 0)
        .tri(0, 0, 0)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    // Facing the s direction: the far edge in s (4 texels), half way in t
    // (2 texels of a 2-texel tile).
    let uv = f.vertices[0].uv[0];
    assert!(
        (uv[0] - 1.0).abs() < 1e-3 && (uv[1] - 1.0).abs() < 1e-3,
        "{uv:?}"
    );
}

#[test]
fn one_command_draws_four_triangles_by_5_bit_vertex_numbers() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 32]);
    // Triangles (1, 2, 31), (4, 5, 6), (7, 8, 9); the fourth place unused
    // (one vertex three times). The first one's third corner is split:
    // 31 = 0b111_11, three bits in the first word, two in the second.
    let w0 = 0x10 << 24 | 1 << 23 | 2 << 18 | 0b111 << 15 | 4 << 10 | 5 << 5 | 6;
    let w1 = 0b11 << 30 | 7 << 25 | 8 << 20 | 9 << 15;
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.vtx(verts, 32, 0).raw(w0, w1).end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert_eq!(f.draws().map(|d| d.count).sum::<u32>(), 9);
}

#[test]
fn a_3d_microcode_followed_in_ram_by_the_2d_one_stays_3d() {
    let mut ram = Ram::default();
    // As in Majora's Mask: the 2D microcode's data, with its own name, right
    // after the 3D one's.
    let data = ram.put(b"RSP Gfx ucode F3DZEX.NoN fifo 2.08J");
    ram.put(b"RSP Gfx ucode S2DEX  fifo 2.05");
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.raw(0xE1 << 24, data)
        .raw(0xDD << 24 | 0x7FF, 0)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    assert_eq!(f.draws().map(|d| d.count).sum::<u32>(), 3);
}

#[test]
fn a_named_microcode_lights_by_distance_from_its_point_lights() {
    let mut ram = Ram::default();
    // The name every microcode carries in its data.
    let data = ram.put(b"RSP Gfx ucode F3DEXBG.NoN fifo 2.08");
    let second = ram.put(b"RSP Gfx ucode F3DEX.NoN   fifo 2.08");
    // Two vertices 100 and 400 units from the light; the colour bytes are
    // a colour.
    let mut vb = Vec::new();
    for x in [100i16, 400] {
        vb.extend_from_slice(&x.to_be_bytes());
        vb.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 128, 0, 200]);
    }
    let verts = ram.put(&vb);
    // 48-byte light places: a white point light of strength 1.0 (0x10) at
    // the origin, no ambient.
    let mut l = vec![255, 255, 255, 0, 255, 255, 255, 0, 0, 0, 127, 0, 0x10];
    l.resize(48, 0);
    let light = ram.put(&l);
    let ambient = ram.put(&[0; 48]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    let place = |n: u32| 0xDC << 24 | 5 << 19 | (n * 48 / 8) << 8 | 10;
    // Light space: clip position (setup's projection scales by 0.01)
    // times 100, back to the vertices' own units.
    let space = |offset: u32, w: u32| (0xDB << 24 | 0x10 << 16 | offset, w);
    for (o, w) in [
        (0, 100 << 16 | 100),
        (4, 100 << 16),
        (8, 100 << 16 | 100),
        (12, 100 << 16),
    ] {
        let (w0, w1) = space(o, w);
        dl.raw(w0, w1);
    }
    dl.geometry(0, G_LIGHTING)
        .raw(0xDB << 24 | 0x02 << 16, 48) // one light
        .raw(place(2), light)
        .raw(place(3), ambient)
        // The game's second 3D microcode, under the usual name: the same
        // variant.
        .raw(0xE1 << 24, second)
        .raw(0xDD << 24 | 0x7FF, 0)
        .vtx(verts, 2, 1)
        .tri(1, 2, 2)
        .end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    it.set_microcode_data(Some(data));
    it.run(&ram.bytes, addr).expect("display list runs");
    let f = it.take_frame();
    // Near: the full vertex colour. Far: 32768 / 400^2 of it.
    let c = f.vertices[0].color;
    assert!(
        (c[0] - 1.0).abs() < 1e-3 && (c[1] - 128.0 / 255.0).abs() < 1e-3,
        "{c:?}"
    );
    assert_eq!((c[2], c[3]), (0.0, 200.0 / 255.0));
    let c = f.vertices[1].color;
    let k = 32768.0 / 160000.0;
    assert!(
        (c[0] - k).abs() < 1e-3 && (c[1] - k * 128.0 / 255.0).abs() < 1e-3,
        "{c:?}"
    );
}

#[test]
fn the_convert_command_sets_k5() {
    let mut ram = Ram::default();
    let verts = ram.vertices(&[v(0, 0, 0, [255; 4]); 3]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    // K5 is the low nine bits, signed: 255 is about one, 0x1FF is minus 1.
    dl.raw(0xEC << 24, 255)
        .vtx(verts, 3, 0)
        .tri(0, 1, 2)
        .raw(0xEC << 24, 0x1FF)
        .tri(0, 1, 2)
        .end();
    let addr = ram.dl(&dl);
    let f = run(&ram, addr);
    let k5: Vec<f32> = f.draws().map(|d| d.state.k5).collect();
    assert_eq!(k5, [1.0, -1.0 / 255.0]);
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
fn a_fill_of_an_8_bit_image_writes_one_byte_per_pixel() {
    let mut ram = Ram::default();
    // An 8-wide, 2-row 8-bit image, then what lies after it in RAM.
    let image = ram.put(&[0; 16]);
    let after = ram.put(&[0xAA; 16]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.cimg8(image, 8)
        .other_modes(CYCLE_FILL, 0)
        .fill_color(0x1122_3344)
        .fill_rect(0, 0, 7, 1)
        .end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    let mut rb = Solid { asked: Vec::new() };
    it.run_readback(&mut ram.bytes, addr, &mut rb).unwrap();
    let at = |a: u32| &ram.bytes[a as usize..a as usize + 16];
    assert_eq!(at(image), [0x11, 0x22, 0x33, 0x44].repeat(4));
    assert_eq!(at(after), [0xAA; 16], "nothing past the image");
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

#[test]
fn a_picture_not_drawn_for_some_frames_is_forgotten_when_asked() {
    for (expire, want) in [(None, 1), (Some(1), 0)] {
        let mut ram = Ram::default();
        let tex = ram.put(&[0x07, 0xC1].repeat(16));
        let drawn = ram.put(&[0; 8 * 6 * 2]);
        // One list draws into the picture...
        let mut dl = Dl::default();
        setup(&mut ram, &mut dl);
        dl.cimg(drawn, 8)
            .other_modes(CYCLE_1, 0)
            .timg(0, 2, 1, tex)
            .tile(0, 2, 0, 0, 7)
            .load_block(7, 16)
            .tile(0, 2, 1, 0, 0)
            .tile_size(0, 4, 4)
            .tex_rect(0, [0, 0, 8, 6], [0, 0], [1 << 10, 1 << 10])
            .end();
        let draw = ram.dl(&dl);
        // ...and a later one, after two frames of nothing, loads from it.
        let mut dl = Dl::default();
        dl.timg(0, 2, 8, drawn).load_block(7, 16).end();
        let load = ram.dl(&dl);
        let mut it = Interpreter::new();
        it.set_readback_frames(expire);
        let mut rb = Solid { asked: Vec::new() };
        it.run_readback(&mut ram.bytes, draw, &mut rb).unwrap();
        for _ in 0..3 {
            it.take_frame();
        }
        it.run_readback(&mut ram.bytes, load, &mut rb).unwrap();
        assert_eq!(rb.asked.len(), want, "expiry {expire:?}");
    }
}

#[test]
fn a_texture_of_another_pixel_size_inside_a_drawn_image_is_not_read_back() {
    let mut ram = Ram::default();
    let drawn = ram.put(&[0; 8 * 6 * 2]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.cimg(drawn, 8)
        .other_modes(CYCLE_1, 0)
        .tex_rect(0, [0, 0, 8, 6], [0, 0], [1 << 10, 1 << 10])
        // An 8-bit texture that lies inside the 16-bit image's span.
        .timg(0, 1, 8, drawn + 32)
        .load_block(7, 16)
        .end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    let mut rb = Solid { asked: Vec::new() };
    it.run_readback(&mut ram.bytes, addr, &mut rb).unwrap();
    assert!(rb.asked.is_empty());
}

/// Notes each read: the image, its size and how many draws it was given.
struct Counting {
    asked: Vec<(u32, u32, u32, usize)>,
}

impl n64rusty_gfx::Readback for Counting {
    fn read(&mut self, f: &Frame, image: u32, width: u32, height: u32) -> Option<Vec<u8>> {
        self.asked.push((image, width, height, f.draws().count()));
        Some([255, 0, 0, 255].repeat((width * height) as usize))
    }
}

#[test]
fn a_narrow_image_at_a_wide_ones_address_reads_back_alone_at_its_scissor_height() {
    let mut ram = Ram::default();
    let img = ram.put(&[0; 16 * 12 * 2]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.cimg(img, 16)
        .other_modes(CYCLE_1, 0)
        .scissor(0, 0, 16, 12)
        .tex_rect(0, [0, 0, 16, 12], [0, 0], [1 << 10, 1 << 10])
        // A small picture drawn at the start of the same buffer, taller
        // than 4:3 of its width.
        .cimg(img, 4)
        .scissor(0, 0, 4, 7)
        .tex_rect(0, [0, 0, 4, 7], [0, 0], [1 << 10, 1 << 10])
        .timg(0, 2, 4, img)
        .load_block(7, 16)
        .end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    let mut rb = Counting { asked: Vec::new() };
    it.run_readback(&mut ram.bytes, addr, &mut rb).unwrap();
    assert_eq!(rb.asked, [(img, 4, 7, 1)]);
}

#[test]
fn strip_loads_of_a_narrow_image_read_it_once_and_never_the_wide_one_at_its_address() {
    let mut ram = Ram::default();
    let img = ram.put(&[0; 16 * 12 * 2]);
    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.cimg(img, 4)
        .other_modes(CYCLE_1, 0)
        .scissor(0, 0, 4, 7)
        .tex_rect(0, [0, 0, 4, 7], [0, 0], [1 << 10, 1 << 10])
        // The full buffer drawn after it, at the same address.
        .cimg(img, 16)
        .scissor(0, 0, 16, 12)
        .tex_rect(0, [0, 0, 16, 12], [0, 0], [1 << 10, 1 << 10])
        // The small picture copied out in two strips, then the full one.
        .timg(0, 2, 4, img)
        .load_block(7, 8)
        .timg(0, 2, 4, img + 4 * 2 * 2)
        .load_block(7, 8)
        .timg(0, 2, 16, img)
        .load_block(7, 16)
        .end();
    let addr = ram.dl(&dl);
    let mut it = Interpreter::new();
    let mut rb = Counting { asked: Vec::new() };
    it.run_readback(&mut ram.bytes, addr, &mut rb).unwrap();
    assert_eq!(rb.asked, [(img, 4, 7, 1), (img, 16, 12, 1)]);
}
