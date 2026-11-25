//! Finds functions in a code image by recursive descent from entry points.
//!
//! Starting from the given entries, every reachable path is followed through
//! branches; `jal` targets and tail-jump targets become new functions. A
//! function's extent is the furthest instruction any of its paths reaches,
//! cut at the next function start. Register jumps are checked for the usual
//! jump table pattern (`lui`, `addu`, `lw`, `jr`) and the table entries become
//! extra block starts.
//!
//! Functions only reached through pointers are found in a second pass: hint
//! addresses (for example from a symbol map read from a local file) and
//! `lui`/`addiu` address pairs that point inside the code found so far and
//! at an instruction that looks like a function prologue.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::decode::{Flow, Inst, Op};
use crate::Function;

/// Code loaded at `base` in guest memory.
#[derive(Clone, Copy)]
pub struct Image<'a> {
    /// Guest address of `bytes[0]`.
    pub base: u32,
    /// Big-endian bytes.
    pub bytes: &'a [u8],
}

impl Image<'_> {
    /// Whether `a` is a word address inside the image.
    pub fn contains(&self, a: u32) -> bool {
        a % 4 == 0 && a >= self.base && ((a - self.base) as usize) + 4 <= self.bytes.len()
    }

    /// The big-endian word at `a`.
    pub fn word(&self, a: u32) -> Option<u32> {
        if !self.contains(a) {
            return None;
        }
        let o = (a - self.base) as usize;
        Some(u32::from_be_bytes(self.bytes[o..o + 4].try_into().unwrap()))
    }
}

/// What discovery found.
#[derive(Clone, Debug)]
pub struct Discovery {
    /// Functions in address order, with jump table targets filled in.
    pub functions: Vec<Function>,
    /// Number of jump tables resolved.
    pub jump_tables: usize,
    /// End of the code reached from the entry points.
    pub text_end: u32,
}

/// How far past its start a `j` may land and still count as a jump inside
/// the same function rather than a tail call.
const INTRA_JUMP_LIMIT: u32 = 0x4000;

struct Explored {
    end: u32,
    calls: Vec<u32>,
    pointers: Vec<u32>,
    jump_targets: Vec<u32>,
    jump_tables: usize,
}

fn explore(img: &Image, s: u32, known: &BTreeSet<u32>) -> Explored {
    let mut work = vec![s];
    let mut visited = HashSet::new();
    let mut ex = Explored {
        end: s,
        calls: Vec::new(),
        pointers: Vec::new(),
        jump_targets: Vec::new(),
        jump_tables: 0,
    };
    let touch = |a: u32, end: &mut u32| *end = (*end).max(a + 4);
    while let Some(b) = work.pop() {
        let mut a = b;
        let mut lui: [Option<u32>; 32] = [None; 32];
        loop {
            if !visited.insert(a) {
                break;
            }
            let Some(w) = img.word(a) else { break };
            touch(a, &mut ex.end);
            let i = Inst::decode(w);
            // Track constants built with lui/addiu/ori, for function
            // pointers and for register jumps to a known address.
            let known_value = match i.op {
                Op::Lui => Some((i.uimm() as u32) << 16),
                Op::Addiu => lui[i.rs as usize].map(|hi| {
                    let v = hi.wrapping_add(i.simm() as u32);
                    ex.pointers.push(v);
                    v
                }),
                Op::Ori => lui[i.rs as usize].map(|hi| hi | i.uimm() as u32),
                _ => None,
            };
            if let Some(d) = i.dest() {
                lui[d as usize] = known_value;
            }
            let delay = a + 4;
            match i.flow() {
                Flow::Next => {
                    a += 4;
                    continue;
                }
                Flow::Branch { link, .. } => {
                    let t = i.branch_target(a);
                    touch(delay, &mut ex.end);
                    visited.insert(delay);
                    // `bal` (bgezal $zero) is a call.
                    if link {
                        if i.rs == 0 && i.op == Op::Bgezal {
                            ex.calls.push(t);
                        }
                    } else if t >= s && img.contains(t) {
                        work.push(t);
                    }
                    let always =
                        (i.op == Op::Beq && i.rs == i.rt) || (i.op == Op::Bgez && i.rs == 0);
                    if always {
                        break;
                    }
                    a += 8;
                }
                Flow::Jump { link: true } => {
                    ex.calls.push(i.jump_target(a));
                    a += 8;
                }
                Flow::JumpReg { link: true } => {
                    if let Some(t) = lui[i.rs as usize] {
                        ex.calls.push(t);
                    }
                    a += 8;
                }
                Flow::Jump { link: false } => {
                    touch(delay, &mut ex.end);
                    let t = i.jump_target(a);
                    if t >= s && t - s < INTRA_JUMP_LIMIT && !known.contains(&t) {
                        work.push(t);
                    } else {
                        ex.calls.push(t);
                    }
                    break;
                }
                Flow::JumpReg { link: false } => {
                    touch(delay, &mut ex.end);
                    if let Some(t) = lui[i.rs as usize].filter(|_| i.rs != 31) {
                        // Jump to a constant address: a tail call.
                        ex.calls.push(t);
                    } else if i.rs != 31 {
                        let entries = jump_table(img, a, i.rs, s);
                        if !entries.is_empty() {
                            ex.jump_tables += 1;
                            ex.jump_targets.extend(&entries);
                            work.extend(entries);
                        }
                    }
                    break;
                }
            }
        }
    }
    ex
}

/// Resolves the table used by `jr $rx` at `a`, if it follows the usual
/// pattern: `sltiu` bound check, `lui base`, `addu base, base, index`,
/// `lw rx, lo(base)`, `jr rx`.
fn jump_table(img: &Image, a: u32, rx: u8, s: u32) -> Vec<u32> {
    let back = |k: u32| {
        a.checked_sub(4 * k)
            .and_then(|x| img.word(x))
            .map(Inst::decode)
    };
    let mut k = 1;
    let (base, lo) = loop {
        let Some(i) = back(k) else { return vec![] };
        if i.op == Op::Lw && i.rt == rx {
            break (i.rs, i.simm() as u32);
        }
        k += 1;
        if k > 16 {
            return vec![];
        }
    };
    let mut hi = None;
    let mut count = None;
    for k in k + 1..k + 32 {
        let Some(i) = back(k) else { break };
        if hi.is_none() && i.op == Op::Lui && i.rt == base {
            hi = Some((i.uimm() as u32) << 16);
        }
        if count.is_none() && i.op == Op::Sltiu {
            count = Some(i.uimm() as u32);
        }
        if hi.is_some() && count.is_some() {
            break;
        }
    }
    let Some(hi) = hi else { return vec![] };
    let table = hi.wrapping_add(lo);
    let limit = count.unwrap_or(1024).min(4096);
    let mut out = Vec::new();
    for n in 0..limit {
        let Some(e) = img.word(table.wrapping_add(4 * n)) else {
            break;
        };
        if !img.contains(e) || e < s || (count.is_none() && e - s >= 0x1_0000) {
            break;
        }
        out.push(e);
    }
    out
}

/// Whether `w` looks like the first instruction of a function:
/// `addiu $sp, $sp, -n`.
fn looks_like_prologue(w: u32) -> bool {
    let i = Inst::decode(w);
    i.op == Op::Addiu && i.rs == 29 && i.rt == 29 && i.simm() < 0
}

/// Whether the code at `a` opens a stack frame in its first four
/// instructions, with no branch or jump before it: compilers often schedule
/// a `lui` or load ahead of the `addiu $sp, $sp, -n`.
fn opens_frame(img: &Image, a: u32) -> bool {
    let plain = |w: u32| {
        let i = Inst::decode(w);
        i.op != Op::Unknown && matches!(i.flow(), Flow::Next)
    };
    (0..4)
        .map_while(|k| img.word(a + 4 * k))
        .take_while(|&w| looks_like_prologue(w) || plain(w))
        .any(looks_like_prologue)
}

/// Whether `a` starts a short function with no stack frame: every word
/// decodes, and a `jr $ra` comes within 32 words. Data between a return
/// and the next one rarely does both.
fn is_leaf(img: &Image, a: u32) -> bool {
    const JR_RA: u32 = 0x03E0_0008;
    let words: Vec<u32> = (0..32).map_while(|k| img.word(a + 4 * k)).collect();
    words.iter().position(|&w| w == JR_RA).is_some_and(|k| {
        words[..=k]
            .iter()
            .all(|&w| Inst::decode(w).op != Op::Unknown)
    })
}

/// Places that start a function by their layout: the image's first word,
/// or the first non-zero word after a `jr $ra` and its delay slot, when it
/// opens a stack frame (see [`opens_frame`]) or is a leaf (see [`is_leaf`]). For code copied in at run
/// time, whose functions are mostly reached through pointers set up by code.
pub fn function_starts(img: &Image) -> Vec<u32> {
    const JR_RA: u32 = 0x03E0_0008;
    let end = img.base.wrapping_add(img.bytes.len() as u32 & !3);
    let mut out = Vec::new();
    let mut a = img.base;
    let mut after_return = true;
    let mut leaves = Vec::new();
    // Every branch as the span it crosses: code inside one still belongs
    // to the function before, even past a return.
    let mut spans = Vec::new();
    while a < end {
        let w = img.word(a).unwrap_or(0);
        if after_return && w != 0 {
            if opens_frame(img, a) {
                out.push(a);
            } else if is_leaf(img, a) {
                leaves.push(a);
            }
            after_return = false;
        }
        let i = Inst::decode(w);
        if matches!(i.flow(), Flow::Branch { link: false, .. }) {
            let t = i.branch_target(a);
            spans.push((a.min(t), a.max(t)));
        }
        if w == JR_RA {
            after_return = true;
            a += 4; // the delay slot
        }
        a += 4;
    }
    out.extend(
        leaves
            .into_iter()
            .filter(|&l| !spans.iter().any(|&(lo, hi)| lo < l && l <= hi)),
    );
    out.sort_unstable();
    out
}

/// Code copies a game's own load table describes, found before any run
/// makes them: `(rom, ram, len)` for each word pair `rom start, rom end`
/// followed within three words by a RAM address at which that ROM range
/// reads as code. It reads as code when its calls into itself land where
/// functions start (after a return and its padding, or at its first word): nine in ten or
/// more, and at least one; code with no such calls must build two or
/// more addresses inside itself (`lui`, `addiu`). Build tools lay segment tables out this way
/// (start, end, then load address), whatever else each entry holds.
pub fn overlay_table(rom: &[u8]) -> Vec<(u32, u32, u32)> {
    const JR_RA: u32 = 0x03E0_0008;
    let words: Vec<u32> = rom
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let rom_len = (words.len() * 4) as u32;
    let mut found: BTreeMap<(u32, u32), Vec<(u32, bool)>> = BTreeMap::new();
    let mut tried = HashSet::new();
    for w in words.windows(5) {
        let (s, e) = (w[0], w[1]);
        if s < 0x1000 || s % 4 != 0 || e <= s || e > rom_len || e - s > 0x10_0000 {
            continue;
        }
        let code = &words[s as usize / 4..e as usize / 4];
        for &ram in &w[2..5] {
            if !(0x8000_0000..0x8080_0000).contains(&ram) || ram % 4 != 0 || !tried.insert((s, ram))
            {
                continue;
            }
            let at = |k: usize| ram.wrapping_add(4 * k as u32);
            // The first word after each return and any padding after it.
            let mut starts = HashSet::from([ram]);
            let mut k = 0;
            while k < code.len() {
                if code[k] == JR_RA {
                    k += 2;
                    while k < code.len() && code[k] == 0 {
                        k += 1;
                    }
                    starts.insert(at(k));
                } else {
                    k += 1;
                }
            }
            let (mut inside, mut good) = (0, 0);
            for &c in code.iter().filter(|&&c| c >> 26 == 3) {
                let t = (ram & 0xF000_0000) | (c & 0x03FF_FFFF) << 2;
                if t >= ram && t - ram < e - s {
                    inside += 1;
                    good += starts.contains(&t) as u32;
                }
            }
            // Code with no calls into itself (leaf functions reached through
            // pointers) still builds addresses inside itself.
            let own_pointers = code
                .windows(2)
                .filter(|p| p[0] >> 26 == 0x0F && p[1] >> 26 == 0x09)
                .filter(|p| (p[0] >> 16) & 31 == (p[1] >> 21) & 31)
                .map(|p| (p[0] << 16).wrapping_add(p[1] as u16 as i16 as u32))
                .filter(|&v| v >= ram && v - ram < e - s)
                .count();
            let by_calls = good > 0 && good * 10 >= inside * 9;
            if by_calls || (inside == 0 && own_pointers >= 2) {
                found.entry((s, e)).or_default().push((ram, by_calls));
                // The load address comes first; later words are often the
                // same code's section addresses, which would also pass.
                break;
            }
        }
    }
    // Every load address that the code's own calls confirm is kept (code
    // can be loaded in two places). Only when none is confirmed that way
    // does the first one confirmed by addresses alone count: a wrong
    // address next to the right one still has addresses landing inside.
    let mut out = Vec::new();
    for ((s, e), rams) in found {
        let mut rams: Vec<u32> = match rams.iter().any(|r| r.1) {
            true => rams.iter().filter(|r| r.1).map(|r| r.0).collect(),
            false => vec![rams[0].0],
        };
        rams.dedup();
        out.extend(rams.into_iter().map(|ram| (s, ram, e - s)));
    }
    out
}

/// Words anywhere in the image that point inside it at something that starts
/// like a function (it opens a stack frame): function pointers in data tables.
/// Pass them to [`discover`] as hints.
pub fn data_pointers(img: &Image) -> Vec<u32> {
    let mut out: Vec<u32> = (0..img.bytes.len() / 4)
        .filter_map(|k| img.word(img.base + 4 * k as u32))
        .filter(|&v| img.contains(v) && opens_frame(img, v))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Finds functions reachable from `entries`, then from `hints` and from
/// address pairs that look like function pointers. Those second-pass
/// addresses must lie below `code_end`, which defaults to the end of the
/// code reached from the entries.
pub fn discover(img: &Image, entries: &[u32], hints: &[u32], code_end: Option<u32>) -> Discovery {
    let mut known: BTreeSet<u32> = BTreeSet::new();
    let mut done: BTreeMap<u32, Explored> = BTreeMap::new();
    let mut queue: Vec<u32> = entries
        .iter()
        .copied()
        .filter(|&a| img.contains(a))
        .collect();
    known.extend(&queue);
    let mut text_end = 0;
    let mut first_pass = true;
    // Past the first pass, calls beyond the code reached from the entries go
    // to code the game loads there later: the image holds other bytes.
    let mut call_end = u32::MAX;
    loop {
        while let Some(s) = queue.pop() {
            if done.contains_key(&s) {
                continue;
            }
            let ex = explore(img, s, &known);
            for &c in &ex.calls {
                if c < call_end && img.contains(c) && known.insert(c) {
                    queue.push(c);
                }
            }
            done.insert(s, ex);
        }
        if first_pass {
            text_end = done.values().map(|e| e.end).max().unwrap_or(img.base);
            first_pass = false;
        }
        let code_end = code_end.unwrap_or(text_end);
        call_end = code_end;
        // Second pass: hints and pointer pairs inside the code found so far,
        // not inside a function already found.
        let inside_known = |a: u32| {
            done.range(..=a)
                .next_back()
                .is_some_and(|(&s, e)| a >= s && a < e.end)
        };
        let pointers: Vec<u32> = done
            .values()
            .flat_map(|e| e.pointers.iter().copied())
            .collect();
        let mut fresh = Vec::new();
        for (a, from_hint) in hints
            .iter()
            .map(|&a| (a, true))
            .chain(pointers.into_iter().map(|a| (a, false)))
        {
            if known.contains(&a) || !img.contains(a) {
                continue;
            }
            // Data follows code, so nothing past the code reached from the
            // entry points is taken. A hint is trusted below that; a pointer
            // pair must also start like a function and not land inside one
            // already found.
            let ok = a < code_end
                && (from_hint || (looks_like_prologue(img.word(a).unwrap()) && !inside_known(a)));
            if !ok {
                continue;
            }
            known.insert(a);
            fresh.push(a);
        }
        // Third: compiled code is laid out function after function, so
        // code in a gap between two functions found so far (after any zero
        // padding) starts a function too: one reached only through a
        // pointer built in code, often a small leaf without a prologue.
        // Only gaps between functions; data follows the last one.
        if fresh.is_empty() {
            let starts: Vec<u32> = done.keys().copied().collect();
            for pair in starts.windows(2) {
                let mut a = done[&pair[0]].end;
                while a < pair[1] && img.word(a) == Some(0) {
                    a += 4;
                }
                // Data that does not even decode is not taken for code.
                let decodes = img
                    .word(a)
                    .is_some_and(|w| Inst::decode(w).op != Op::Unknown);
                if a < pair[1] && a < code_end && decodes && !inside_known(a) && known.insert(a) {
                    fresh.push(a);
                }
            }
        }
        if fresh.is_empty() {
            break;
        }
        queue = fresh;
    }

    let starts: Vec<u32> = done.keys().copied().collect();
    let mut functions = Vec::with_capacity(starts.len());
    let mut jump_tables = 0;
    for (n, &s) in starts.iter().enumerate() {
        let ex = &done[&s];
        jump_tables += ex.jump_tables;
        let next = starts.get(n + 1).copied().unwrap_or(u32::MAX);
        let end = ex.end.min(next).max(s + 4);
        let words = (s..end).step_by(4).map_while(|a| img.word(a)).collect();
        let mut f = Function::new(s, words);
        f.jump_targets = ex
            .jump_targets
            .iter()
            .copied()
            .filter(|&t| t < end)
            .collect();
        f.jump_targets.sort_unstable();
        f.jump_targets.dedup();
        functions.push(f);
    }
    Discovery {
        functions,
        jump_tables,
        text_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x8000_0400;

    fn image(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    // Hand-encoded helpers.
    fn jal(t: u32) -> u32 {
        0x0C00_0000 | ((t >> 2) & 0x03FF_FFFF)
    }
    const JR_RA: u32 = 0x03E0_0008;
    const NOP: u32 = 0;

    #[test]
    fn function_starts_follow_returns() {
        const FRAME: u32 = 0x27BD_FFF8; // addiu $sp,$sp,-8
                                        // A function, padding, a function, a leaf (no frame), then data
                                        // after a return that does not open a frame.
        let words = [
            FRAME,
            JR_RA,
            NOP,
            NOP,
            NOP, // 0x00, padding at 0x0c
            FRAME,
            JR_RA,
            0x27BD_0008, // 0x14, delay slot pops the frame
            0x2402_0001,
            JR_RA,
            NOP, // 0x20: leaf
            0x1234_5678,
            FRAME, // data, then a frame-like word not after a return
        ];
        let bytes = image(&words);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        // The leaf counts; the data after it reaches no return.
        assert_eq!(function_starts(&img), [BASE, BASE + 0x14, BASE + 0x20]);
        // Code after a return that a branch before it reaches is not a leaf.
        let words = [0x1080_0003, NOP, JR_RA, NOP, 0x2402_0001, JR_RA, NOP];
        let bytes = image(&words);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        assert_eq!(function_starts(&img), [BASE]);
        // Nor is code after a return that a later branch loops back over.
        let words = [JR_RA, NOP, 0x2402_0001, JR_RA, NOP, 0x1000_FFFB, NOP];
        let bytes = image(&words);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        assert_eq!(function_starts(&img), [BASE]);
        // A frame opened after a scheduled `lui` still starts a function.
        let words = [JR_RA, NOP, 0x3C0E_800A, FRAME, JR_RA, NOP];
        let bytes = image(&words);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        assert_eq!(function_starts(&img), [BASE, BASE + 8]);
    }

    #[test]
    fn data_pointers_find_prologues() {
        // 0x1000: addiu $sp,$sp,-8; 0x1004: jr $ra; 0x1008: nop;
        // 0x100c: pointer to 0x1000; 0x1010: pointer to 0x1004 (no prologue).
        let words = [0x27BD_FFF8u32, 0x03E0_0008, 0, 0x1000, 0x1004];
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        let img = Image {
            base: 0x1000,
            bytes: &bytes,
        };
        assert_eq!(data_pointers(&img), vec![0x1000]);
    }

    #[test]
    fn follows_calls_and_ends_at_return() {
        // f: jal g ; nop ; jr ra ; nop | g: jr ra ; nop | (data)
        let bytes = image(&[jal(BASE + 16), NOP, JR_RA, NOP, JR_RA, NOP, 0xFFFF_FFFF]);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        let d = discover(&img, &[BASE], &[], None);
        let got: Vec<(u32, usize)> = d
            .functions
            .iter()
            .map(|f| (f.addr, f.words.len()))
            .collect();
        assert_eq!(got, vec![(BASE, 4), (BASE + 16, 2)]);
        assert_eq!(d.text_end, BASE + 24);
    }

    #[test]
    fn code_between_functions_is_a_function() {
        // f: jal h ; nop ; jr ra ; nop | pad | g (reached by no call):
        // jr ra ; nop | h: jr ra ; nop | (data after the last function)
        let bytes = image(&[
            jal(BASE + 32),
            NOP,
            JR_RA,
            NOP,
            0,
            0,
            JR_RA,
            NOP,
            JR_RA,
            NOP,
            0x1234_5678,
            JR_RA,
        ]);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        let d = discover(&img, &[BASE], &[], None);
        let starts: Vec<u32> = d.functions.iter().map(|f| f.addr).collect();
        assert_eq!(starts, vec![BASE, BASE + 24, BASE + 32]);
    }

    #[test]
    fn data_between_functions_is_not_a_function() {
        // f: jal h ; nop ; jr ra ; nop | a word that is no instruction |
        // h: jr ra ; nop
        let bytes = image(&[jal(BASE + 20), NOP, JR_RA, NOP, 0x7000_0000, JR_RA, NOP]);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        assert_eq!(Inst::decode(0x7000_0000).op, Op::Unknown);
        let d = discover(&img, &[BASE], &[], None);
        let starts: Vec<u32> = d.functions.iter().map(|f| f.addr).collect();
        assert_eq!(starts, vec![BASE, BASE + 20]);
    }

    #[test]
    fn pointer_found_code_does_not_call_past_the_code() {
        // f: jr ra ; nop | g (only a hint): jal past ; nop ; jr ra ; nop |
        // past the code: bytes loaded over later, which here call into g.
        let past = BASE + 32;
        let bytes = image(&[
            JR_RA,
            NOP,
            jal(past),
            NOP,
            JR_RA,
            NOP,
            0,
            0,
            jal(BASE + 12),
            NOP,
            JR_RA,
            NOP,
        ]);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        let d = discover(&img, &[BASE], &[BASE + 8], Some(BASE + 24));
        let starts: Vec<u32> = d.functions.iter().map(|f| f.addr).collect();
        assert_eq!(starts, vec![BASE, BASE + 8]);
    }

    #[test]
    fn branches_extend_the_function() {
        // beq a0, zero, +3 ; nop ; jr ra ; nop ; jr ra ; nop
        let bytes = image(&[0x1080_0002, NOP, JR_RA, NOP, JR_RA, NOP]);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        let d = discover(&img, &[BASE], &[], None);
        assert_eq!(d.functions.len(), 1);
        assert_eq!(d.functions[0].words.len(), 6);
    }

    #[test]
    fn resolves_a_jump_table() {
        // 0: sltiu at, a0, 2
        // 1: lui t0, %hi(table)
        // 2: sll t1, a0, 2
        // 3: addu t0, t0, t1
        // 4: lw t2, %lo(table)(t0)
        // 5: jr t2
        // 6: nop
        // 7: jr ra ; 8: nop      (case 0)
        // 9: jr ra ; 10: nop     (case 1)
        // 11, 12: table
        let table = BASE + 44;
        let words = [
            0x2C81_0002,
            0x3C08_0000 | (table >> 16),
            0x0004_4880,
            0x0109_4021,
            0x8D0A_0000 | (table & 0xFFFF),
            0x0140_0008,
            NOP,
            JR_RA,
            NOP,
            JR_RA,
            NOP,
            BASE + 28,
            BASE + 36,
        ];
        let bytes = image(&words);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        let d = discover(&img, &[BASE], &[], None);
        assert_eq!(d.jump_tables, 1);
        assert_eq!(d.functions[0].jump_targets, vec![BASE + 28, BASE + 36]);
        assert_eq!(d.functions[0].words.len(), 11);
    }

    #[test]
    fn pointer_pairs_need_a_prologue() {
        // f: lui a0, hi(g) ; addiu a0, a0, lo(g) ; jr ra ; nop
        // g: addiu sp, sp, -8 ; jr ra ; addiu sp, sp, 8
        let g = BASE + 16;
        let bytes = image(&[
            0x3C04_0000 | (g >> 16),
            0x2484_0000 | (g & 0xFFFF),
            JR_RA,
            NOP,
            0x27BD_FFF8,
            JR_RA,
            0x27BD_0008,
        ]);
        let img = Image {
            base: BASE,
            bytes: &bytes,
        };
        // g lies past the code reached from the entry: a hint finds it only
        // when the caller says the code goes on that far.
        let d = discover(&img, &[BASE], &[], None);
        assert_eq!(d.functions.len(), 1);
        let d = discover(&img, &[BASE], &[g], Some(BASE + 28));
        assert_eq!(d.functions.len(), 2);
        let d = discover(&img, &[BASE], &[g], None);
        assert_eq!(d.functions.len(), 1);
        // The lui/addiu pair alone finds it too, since it has a prologue.
        let d = discover(&img, &[BASE], &[], Some(BASE + 28));
        assert_eq!(d.functions.len(), 2);
    }
    #[test]
    fn overlay_table_finds_code_by_its_own_calls() {
        // ROM: a table entry at 0x1000 (start 0x2000, end 0x2018, load
        // address 0x8020_0000), and at 0x2000 two functions, the first
        // calling the second (jal 0x8020_000c).
        let mut rom = vec![0u8; 0x3000];
        let put = |rom: &mut Vec<u8>, at: usize, w: u32| {
            rom[at..at + 4].copy_from_slice(&w.to_be_bytes())
        };
        for (k, w) in [0x2000u32, 0x2018, 0x8020_0000].into_iter().enumerate() {
            put(&mut rom, 0x1000 + 4 * k, w);
        }
        let jal = 0x0C00_0000 | (0x8020_000c & 0x03FF_FFFF) >> 2;
        for (k, w) in [jal, JR_RA, NOP, 0x2402_0001, JR_RA, NOP]
            .into_iter()
            .enumerate()
        {
            put(&mut rom, 0x2000 + 4 * k, w);
        }
        assert_eq!(overlay_table(&rom), [(0x2000, 0x8020_0000, 0x18)]);
        // The same code read at the wrong address calls into its middle.
        put(&mut rom, 0x1008, 0x8020_0004);
        assert_eq!(overlay_table(&rom), []);
    }
    #[test]
    fn overlay_table_prefers_addresses_its_calls_confirm() {
        // Two table entries for the code at ROM 0x2000..0x2040: first a wrong
        // load address 0x8020_0020, then the right one 0x8020_0000. The code
        // calls 0x8020_000c and builds 0x8020_0030 and 0x8020_0034.
        let mut rom = vec![0u8; 0x3000];
        let put = |rom: &mut Vec<u8>, at: usize, w: u32| {
            rom[at..at + 4].copy_from_slice(&w.to_be_bytes())
        };
        for (k, w) in [
            0x2000u32,
            0x2040,
            0x8020_0020,
            0,
            0,
            0x2000,
            0x2040,
            0x8020_0000,
        ]
        .into_iter()
        .enumerate()
        {
            put(&mut rom, 0x1000 + 4 * k, w);
        }
        let jal = 0x0C00_0000 | (0x8020_000c & 0x03FF_FFFF) >> 2;
        let code = [
            jal,
            JR_RA,
            NOP,
            0x2402_0001,
            JR_RA,
            NOP,
            0x3C04_8020,
            0x2484_0030,
            0x3C05_8020,
            0x24A5_0034,
            JR_RA,
            NOP,
        ];
        for (k, w) in code.into_iter().enumerate() {
            put(&mut rom, 0x2000 + 4 * k, w);
        }
        assert_eq!(overlay_table(&rom), [(0x2000, 0x8020_0000, 0x40)]);
    }
}
