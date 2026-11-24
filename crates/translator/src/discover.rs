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
    loop {
        while let Some(s) = queue.pop() {
            if done.contains_key(&s) {
                continue;
            }
            let ex = explore(img, s, &known);
            for &c in &ex.calls {
                if img.contains(c) && known.insert(c) {
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
}
