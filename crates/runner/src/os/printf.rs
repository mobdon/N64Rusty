//! The guest's `printf`-style formatting, for debug text a game prints.

use n64rusty_runtime::Ctx;

use super::arg;

/// The NUL-terminated string at `at` (at most 1 KiB).
fn guest_str(c: &Ctx, at: u32) -> String {
    (0..1024)
        .map(|i| c.mem.read_u8(at.wrapping_add(i)))
        .take_while(|&b| b != 0)
        .map(char::from)
        .collect()
}

/// Formats the guest `printf` format string at `fmt` with the arguments
/// from argument slot `first` on: %d %i %u %x %X %o %c %s %p %f %e %g,
/// with flags, width and precision as written.
pub(super) fn format_guest(c: &Ctx, fmt: u32, first: usize) -> String {
    let f = guest_str(c, fmt);
    let mut out = String::new();
    let mut k = first;
    let mut chars = f.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            chars.next();
            spec.push(n);
            if n.is_ascii_alphabetic() && n != 'l' && n != 'h' || n == '%' {
                break;
            }
        }
        let conv = spec.pop().unwrap_or('%');
        let width: usize = spec
            .trim_start_matches(['-', '0', '+', ' ', '#'])
            .split('.')
            .next()
            .and_then(|w| w.trim_end_matches(['l', 'h']).parse().ok())
            .unwrap_or(0);
        let zero = spec.starts_with('0');
        let left = spec.starts_with('-');
        let prec: Option<usize> = spec
            .split_once('.')
            .and_then(|(_, p)| p.trim_end_matches(['l', 'h']).parse().ok());
        let mut next = || {
            k += 1;
            arg(c, k - 1)
        };
        let body = match conv {
            '%' => "%".to_string(),
            'd' | 'i' => (next() as i32).to_string(),
            'u' => next().to_string(),
            'x' => format!("{:x}", next()),
            'X' => format!("{:X}", next()),
            'o' => format!("{:o}", next()),
            'p' => format!("{:08x}", next()),
            'c' => char::from(next() as u8).to_string(),
            's' => {
                let t = guest_str(c, next());
                match prec {
                    Some(p) => t.chars().take(p).collect(),
                    None => t,
                }
            }
            'f' | 'e' | 'g' | 'F' | 'E' | 'G' => {
                // Doubles take an even pair of argument slots.
                k += k % 2;
                let hi = u64::from(arg(c, k));
                let lo = u64::from(arg(c, k + 1));
                k += 2;
                let v = f64::from_bits(hi << 32 | lo);
                c_float(v, conv, prec.unwrap_or(6))
            }
            other => format!("%{spec}{other}"),
        };
        let pad = width.saturating_sub(body.chars().count());
        if left {
            out.push_str(&body);
            out.extend(std::iter::repeat_n(' ', pad));
        } else if zero && conv != 's' && conv != 'c' {
            // Zeros go after the sign.
            let digits = body.strip_prefix('-').map_or(&body[..], |d| {
                out.push('-');
                d
            });
            out.extend(std::iter::repeat_n('0', pad));
            out.push_str(digits);
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(&body);
        }
    }
    out
}

/// `v` as C's `printf` writes it with conversion `conv` (f e g F E G) and
/// precision `prec`.
pub(super) fn c_float(v: f64, conv: char, prec: usize) -> String {
    // C writes the exponent with a sign and at least two digits.
    let exp = |p: usize| {
        let t = format!("{v:.p$e}");
        let (m, e) = t.split_once('e').unwrap_or((&t, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    };
    let t = match conv.to_ascii_lowercase() {
        'e' => exp(prec),
        'g' => {
            let p = prec.max(1);
            let x = if v == 0.0 {
                0
            } else {
                format!("{v:.*e}", p - 1)
                    .split_once('e')
                    .and_then(|(_, e)| e.parse::<i32>().ok())
                    .unwrap_or(0)
            };
            // Trailing zeros (and a bare point) are dropped.
            let trim = |t: String| {
                if t.contains('.') {
                    t.trim_end_matches('0').trim_end_matches('.').to_string()
                } else {
                    t
                }
            };
            if x < -4 || x >= p as i32 {
                let t = exp(p - 1);
                let (m, e) = t.split_once('e').unwrap_or((&t, ""));
                format!("{}e{e}", trim(m.to_string()))
            } else {
                trim(format!("{v:.*}", (p as i32 - 1 - x).max(0) as usize))
            }
        }
        _ => format!("{v:.prec$}"),
    };
    if conv.is_ascii_uppercase() {
        t.to_uppercase()
    } else {
        t
    }
}
