//! Fingerprints of library routines, to find them in a ROM with no symbol
//! map. A fingerprint is a routine's length and a hash of its code with what
//! relocation changes taken out ([`masked`]); it holds no code. Lists of
//! fingerprints are made from a game whose routines are known and kept
//! locally, like symbol maps.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// An instruction word with what relocation changes taken out: jump
/// targets, `lui` values and the offsets of addressing not based on `sp`.
pub fn masked(w: u32) -> u32 {
    match w >> 26 {
        2 | 3 => w & 0xFC00_0000,
        0x0F => w & 0xFFFF_0000,
        0x09 | 0x0D | 0x20..=0x2F | 0x31 | 0x35 | 0x37 | 0x39 | 0x3D | 0x3F
            if (w >> 21) & 31 != 29 =>
        {
            w & 0xFFFF_0000
        }
        _ => w,
    }
}

/// Whether word `i` can start a routine: it is the first word, or the words
/// before it (past any padding) end one with `jr ra` or a `j` and its delay slot.
/// Short routines otherwise match the tail of a longer one.
pub fn starts_function(w: &[u32], i: usize) -> bool {
    let ends = |k: usize| w[k] == 0x03E0_0008 || w[k] >> 26 == 2;
    let mut j = i;
    while j > 0 && w[j - 1] == 0 {
        j -= 1;
    }
    i == 0
        || (i >= 2 && ends(i - 2))
        || (j < i && ((j >= 1 && ends(j - 1)) || (j >= 2 && ends(j - 2))))
}

/// Big-endian words of `bytes`.
pub fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

/// FNV-1a, one masked word at a time.
fn step(h: u64, w: u32) -> u64 {
    w.to_be_bytes()
        .iter()
        .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3))
}
const SEED: u64 = 0xCBF2_9CE4_8422_2325;

/// Shortest routine worth a fingerprint: shorter code matches by chance.
const MIN_LEN: usize = 6;

/// Fingerprint lines for `code` (words loaded at `base`) whose routines
/// `names` gives, each running to the next name, trailing padding left out:
/// `name len hash` for each routine in `wanted`, and `call len hash offset
/// name` for each call from any routine to one in `wanted`, so routines too
/// short to fingerprint are found through a caller. Callers stay unnamed.
pub fn make(
    code: &[u32],
    base: u32,
    names: &BTreeMap<u32, String>,
    wanted: &BTreeSet<String>,
) -> String {
    let end = base + 4 * code.len() as u32;
    let starts: Vec<u32> = names
        .keys()
        .copied()
        .filter(|a| (base..end).contains(a))
        .collect();
    let mut out = String::new();
    for (k, a) in starts.iter().enumerate() {
        let at = ((a - base) / 4) as usize;
        let next = starts
            .get(k + 1)
            .map_or(code.len(), |n| ((n - base) / 4) as usize);
        let mut len = (next - at).min(0x1000);
        while len > 0 && code[at + len - 1] == 0 {
            len -= 1;
        }
        if len < MIN_LEN {
            continue;
        }
        let body = &code[at..at + len];
        let h = body.iter().fold(SEED, |h, &w| step(h, masked(w)));
        if wanted.contains(&names[a]) {
            out += &format!("{} {len} {h:016x}\n", names[a]);
        }
        for (off, &w) in body.iter().enumerate() {
            let callee = names.get(&jal_target(w).unwrap_or(0));
            if let Some(n) = callee.filter(|n| wanted.contains(*n)) {
                out += &format!("call {len} {h:016x} {off} {n}\n");
            }
        }
    }
    out
}

/// What a fingerprint match means: `Ok(name)` the routine itself,
/// `Err((offset, name))` a call to `name` at that word.
type Meaning<'a> = Result<&'a str, (usize, &'a str)>;

fn jal_target(w: u32) -> Option<u32> {
    (w >> 26 == 3).then_some(0x8000_0000 | (w & 0x03FF_FFFF) << 2)
}

/// Finds routines from fingerprint lines in `code` (words loaded at
/// `base`): a name is given an address when everything that points at it
/// (its own fingerprints, calls from matched callers) points at one
/// address, and no other name is given that address.
pub fn find(prints: &str, code: &[u32], base: u32) -> BTreeMap<u32, String> {
    let mut by_print: HashMap<(usize, u64), Vec<Meaning>> = HashMap::new();
    for line in prints.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (print, what) = match f[..] {
            ["call", len, h, off, n] => ((len, h), off.parse().map(|o| Err((o, n)))),
            [n, len, h] => ((len, h), Ok(Ok(n))),
            _ => continue,
        };
        let (Ok(len), Ok(h), Ok(what)) = (print.0.parse(), u64::from_str_radix(print.1, 16), what)
        else {
            continue;
        };
        by_print.entry((len, h)).or_default().push(what);
    }
    let max = by_print.keys().map(|k| k.0).max().unwrap_or(0);
    // Name to address to (own fingerprint matches, caller matches).
    let mut hits: BTreeMap<&str, BTreeMap<u32, (u32, u32)>> = BTreeMap::new();
    for i in (0..code.len()).filter(|&i| starts_function(code, i)) {
        let mut h = SEED;
        for (len, &w) in code[i..code.len().min(i + max)].iter().enumerate() {
            h = step(h, masked(w));
            for what in by_print.get(&(len + 1, h)).into_iter().flatten() {
                let (n, at) = match *what {
                    Ok(n) => (n, Some(base + 4 * i as u32)),
                    Err((off, n)) => (n, code.get(i + off).copied().and_then(jal_target)),
                };
                if let Some(at) = at {
                    let e = hits.entry(n).or_default().entry(at).or_default();
                    if what.is_ok() {
                        e.0 += 1;
                    } else {
                        e.1 += 1;
                    }
                }
            }
        }
    }
    // A routine's own fingerprint decides when it matches once; otherwise
    // the address most matched callers call, if one has the most. Callers
    // from other library versions can call another routine at the same place.
    let mut found: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
    for (n, at) in hits {
        let own: Vec<u32> = at.iter().filter(|e| e.1 .0 > 0).map(|e| *e.0).collect();
        let pick = match own[..] {
            [a] => Some(a),
            [] => {
                let best = at.values().map(|v| v.1).max().unwrap_or(0);
                let top: Vec<u32> = at.iter().filter(|e| e.1 .1 == best).map(|e| *e.0).collect();
                (top.len() == 1).then(|| top[0])
            }
            _ => None,
        };
        if let Some(a) = pick {
            found.entry(a).or_default().push(n);
        }
    }
    found
        .into_iter()
        .filter(|(_, n)| n.len() == 1)
        .map(|(a, n)| (a, n[0].to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_relocated_routine_by_its_fingerprint() {
        const B: u32 = 0x8000_0400;
        // f: lui a0,X; lw v0,X(a0); addiu v0,v0,1; sw v0,X(a0); addiu v0,v0,2; jr ra; nop
        let f = |x: u32| {
            [
                0x3C04_0000 | x,
                0x8C82_0000 | x,
                0x2442_0001,
                0xAC82_0000 | x,
                0x2442_0002,
                0x03E0_0008,
                0,
            ]
        };
        let mut reference = vec![0x03E0_0008, 0];
        reference.extend(f(0x10));
        let names = [(B + 8, "f".to_string())].into_iter().collect();
        let prints = make(&reference, B, &names, &["f".to_string()].into());
        // The same code with other addresses, after other code.
        let mut target = vec![0x2402_0001, 0x03E0_0008, 0, 0];
        target.extend(f(0x20));
        assert_eq!(find(&prints, &target, B)[&(B + 16)], "f");
        // Found twice: not named.
        target.extend(f(0x30));
        assert!(find(&prints, &target, B).is_empty());
    }

    #[test]
    fn finds_a_short_routine_through_its_caller() {
        const B: u32 = 0x8000_0400;
        let jal = |t: u32| 0x0C00_0000 | (t & 0x0FFF_FFFF) >> 2;
        // c (not wanted): five words then jal g; g: jr ra; addiu v0,zero,1
        let program = |g: u32| {
            let mut v = vec![
                0x27BD_FFE8,
                0xAFBF_0014,
                0x2404_0001,
                0x2405_0002,
                0x2406_0003,
            ];
            v.extend([jal(g), 0, 0x8FBF_0014, 0x03E0_0008, 0x27BD_0018]);
            v.extend([0x03E0_0008, 0x2402_0001]);
            v
        };
        let names = [(B, "c".to_string()), (B + 40, "g".to_string())]
            .into_iter()
            .collect();
        let prints = make(&program(B + 40), B, &names, &["g".to_string()].into());
        assert!(prints.starts_with("call 10 "), "{prints}");
        let mut target = vec![0x03E0_0008, 0];
        target.extend(program(B + 48));
        assert_eq!(find(&prints, &target, B)[&(B + 48)], "g");
    }
}
