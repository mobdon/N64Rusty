//! Renders a hand-built display list headlessly and saves it as a PNG.
//!
//! ```text
//! cargo run -p n64rusty-gfx --example synthetic -- out.png [scale]
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;
use n64rusty_gfx::{write_png, Combiner, Interpreter, Renderer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "synthetic.png".into());
    let scale: u32 = args.next().map_or(Ok(2), |s| s.parse())?;

    let mut ram = Ram::default();
    // A 4x4 RGBA16 checker, one 64-bit line per row.
    let mut tex = Vec::new();
    for y in 0..4 {
        for x in 0..4 {
            let c: u16 = if (x + y) % 2 == 0 { 0xFFC1 } else { 0x3A0F };
            tex.extend_from_slice(&c.to_be_bytes());
        }
    }
    let tex = ram.put(&tex);
    let tri = ram.vertices(&[
        v(-90, -70, 10, [255, 0, 0, 255]),
        v(-10, -70, 10, [0, 255, 0, 255]),
        v(-50, 60, 10, [0, 0, 255, 255]),
    ]);
    let mut quad = [
        v(10, -70, 20, [255; 4]),
        v(90, -70, 20, [255; 4]),
        v(90, 60, 20, [255; 4]),
        v(10, 60, 20, [255; 4]),
    ];
    for (q, st) in quad.iter_mut().zip([[0, 8], [8, 8], [8, 0], [0, 0]]) {
        q.st = [st[0] << 5, st[1] << 5];
    }
    let quad = ram.vertices(&quad);
    let glass = ram.vertices(&[
        v(-60, -20, -10, [255, 255, 255, 96]),
        v(60, -20, -10, [255, 255, 255, 96]),
        v(60, 20, -10, [255, 255, 255, 96]),
        v(-60, 20, -10, [255, 255, 255, 96]),
    ]);

    let mut dl = Dl::default();
    setup(&mut ram, &mut dl);
    dl.other_modes(CYCLE_FILL, 0)
        .cimg(0x0E_0000, 320)
        .fill_color(0xFFFC_FFFC)
        .fill_rect(0, 0, 319, 239)
        .cimg(0x0F_0000, 320)
        .fill_color(0x18C7_18C7)
        .fill_rect(0, 0, 319, 239)
        // Gouraud-shaded triangle.
        .other_modes(CYCLE_1, ZBUF)
        .geometry(0, G_ZBUFFER | G_SHADE | G_SHADING_SMOOTH)
        .combine(pass(SHADE))
        .vtx(tri, 3, 0)
        .tri(0, 1, 2)
        // Texture times primitive colour, point sampled, repeating twice.
        .combine(Combiner::both([TEXEL0, 15, PRIM, 7], [TEXEL0, 7, PRIM, 7]).to_raw())
        .prim(0xFFFF_FFFF)
        .texture(true, 0)
        .timg(0, 2, 1, tex)
        .tile(0, 2, 0, 0, 7)
        .load_block(7, 16)
        .tile(0, 2, 1, 0, 0)
        .tile_size(0, 4, 4)
        .vtx(quad, 4, 0)
        .tri2([0, 1, 2], [0, 2, 3])
        // A translucent bar in front of both.
        .texture(false, 0)
        .other_modes(CYCLE_1, ZBUF | XLU)
        .combine(pass(SHADE))
        .vtx(glass, 4, 0)
        .tri2([0, 1, 2], [0, 2, 3])
        .end();
    let addr = ram.dl(&dl);

    let mut it = Interpreter::new();
    it.run(&ram.bytes, addr)?;
    let frame = it.take_frame();
    let mut renderer = Renderer::new()?;
    let image = renderer.render(&frame, 320 * scale, 240 * scale)?;
    write_png(out.as_ref(), &image)?;
    println!(
        "{out}: {} via {}",
        frame.draws().count(),
        renderer.adapter_name()
    );
    Ok(())
}
