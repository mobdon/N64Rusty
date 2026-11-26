//! Draws one display list from a saved RAM image (a run's `--ram-out`) and
//! saves it as a PNG, to check a graphics change without running the game.
//!
//! ```text
//! cargo run --release -p n64rusty-gfx --example replay -- ram.bin DL out.png [options]
//! ```
//!
//! `DL` is the display list's address in hex (a run's report names the last
//! one). Options:
//!
//! - `--ucode ADDR`: the task's microcode data, so the command set follows
//!   its name (as a run does).
//! - `--scale N`: draw at N times the game's resolution (default 2).
//! - `--only A-B` or `--skip A-B`: keep or drop draws A to B (numbers as
//!   printed), to find which draws make a part of the picture.
//! - `--textures DIR`: also save each texture the frame uses as a PNG.
//! - `--soft`: the console's soft texture filtering instead of crisp texels.
//!
//! It prints each draw's number, triangle count, textures, combiner and
//! geometry mode.

use n64rusty_gfx::{write_png, Command, Image, Interpreter, Renderer};
use std::path::PathBuf;

fn hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("{s}: {e}"))
}

fn range(s: &str) -> Result<(usize, usize), String> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let n = |t: &str| t.parse::<usize>().map_err(|e| format!("{s}: {e}"));
    Ok((n(a)?, n(b)?))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [ram, dl, out, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: replay ram.bin DL out.png [--ucode ADDR] [--scale N] \
                    [--only A-B | --skip A-B] [--textures DIR]"
                .into(),
        );
    };
    let (mut ucode, mut scale, mut keep, mut drop, mut textures) = (None, 2, None, None, None);
    let mut soft = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--soft" {
            soft = true;
            continue;
        }
        let mut val = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--ucode" => ucode = Some(hex(val()?)? & 0x00FF_FFFF),
            "--scale" => scale = val()?.parse()?,
            "--only" => keep = Some(range(val()?)?),
            "--skip" => drop = Some(range(val()?)?),
            "--textures" => textures = Some(PathBuf::from(val()?)),
            _ => return Err(format!("unknown option {a}").into()),
        }
    }
    let bytes = std::fs::read(ram)?;
    let mut interp = Interpreter::new();
    interp.set_microcode_data(ucode);
    interp.run(&bytes, hex(dl)? & 0x00FF_FFFF)?;
    let mut frame = interp.take_frame();
    for w in &frame.warnings {
        println!("skipped: {w}");
    }
    // Number the draws, then keep only the chosen ones.
    let mut n = 0;
    frame.commands.retain(|c| {
        let Command::Draw(d) = c else { return true };
        let k = n;
        n += 1;
        let s = &d.state;
        // The average shade colour, as 0-255.
        let vs = &frame.vertices[d.first as usize..(d.first + d.count) as usize];
        let avg: Vec<u32> = (0..4)
            .map(|c| (vs.iter().map(|v| v.color[c]).sum::<f32>() / vs.len().max(1) as f32 * 255.0) as u32)
            .collect();
        // The range of the first tile's texture coordinates, in texture
        // widths (1.0 spans the texture once).
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for v in vs {
            for c in 0..2 {
                lo[c] = lo[c].min(v.uv[0][c]);
                hi[c] = hi[c].max(v.uv[0][c]);
            }
        }
        let tex = s.textures[0]
            .and_then(|t| frame.textures.get(t as usize))
            .map_or(String::new(), |t| {
                format!(" tex0 {}x{} {:?}", t.width, t.height, t.wrap)
            });
        let uv = format!(
            "uv {:.2}..{:.2} x {:.2}..{:.2}{tex}",
            lo[0], hi[0], lo[1], hi[1]
        );
        println!(
            "draw {k:4}: {:4} tris, textures {:?}, combine {:016x}, geometry {:#08x}, shade {avg:?}, {uv}{}",
            d.count / 3,
            s.textures,
            s.combine,
            s.geometry,
            if s.rect { ", rect" } else { "" }
        );
        let inside = |r: Option<(usize, usize)>| r.is_some_and(|(a, b)| (a..=b).contains(&k));
        keep.is_none_or(|_| inside(keep)) && !inside(drop)
    });
    let mut renderer = Renderer::new()?;
    renderer.set_sharp_textures(!soft);
    let image = renderer.render(&frame, frame.width * scale, frame.height() * scale)?;
    write_png(out.as_ref(), &image)?;
    println!("{out}: {n} draws via {}", renderer.adapter_name());
    if let Some(dir) = textures {
        std::fs::create_dir_all(&dir)?;
        for (k, t) in frame.textures.iter().enumerate() {
            let image = Image {
                width: t.width,
                height: t.height,
                rgba: t.rgba.clone(),
            };
            write_png(&dir.join(format!("tex_{k:03}.png")), &image)?;
        }
        println!("{} textures in {}", frame.textures.len(), dir.display());
    }
    Ok(())
}
