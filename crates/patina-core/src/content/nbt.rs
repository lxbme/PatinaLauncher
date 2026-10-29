//! A minimal, hostile-input-safe NBT reader.
//!
//! Only `root -> Data -> LevelName (String)` and `LastPlayed (Long)` are
//! extracted. The walk is iterative (explicit stack, depth-limited), every
//! length is checked against the bytes that remain, and nothing is allocated
//! from a declared length. Unlike a recursive deserializer it cannot overflow
//! the native stack on deeply nested input.

/// Maximum compound/list nesting accepted.
const MAX_DEPTH: usize = 512;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LevelMeta {
    pub level_name: Option<String>,
    pub last_played: Option<i64>,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    fn i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i64(&mut self) -> Option<i64> {
        let b = self.take(8)?;
        Some(i64::from_be_bytes(b.try_into().ok()?))
    }

    /// A non-negative element count.
    fn len(&mut self) -> Option<usize> {
        usize::try_from(self.i32()?).ok()
    }

    fn name(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()?;
        self.take(n)
    }
}

enum Frame {
    Compound { root: bool, data: bool },
    List { ty: u8, left: usize },
}

/// Parses uncompressed NBT. `None` when the buffer does not start with a
/// named root compound, or fails before yielding either field. Fields seen
/// before a later corruption are kept.
pub(crate) fn read_level_meta(buf: &[u8]) -> Option<LevelMeta> {
    let mut r = Reader { buf, pos: 0 };
    if r.u8()? != 10 {
        return None;
    }
    r.name()?;
    let mut meta = LevelMeta::default();
    let ok = walk(&mut r, &mut meta).is_some();
    (ok || meta.level_name.is_some() || meta.last_played.is_some()).then_some(meta)
}

fn walk(r: &mut Reader, meta: &mut LevelMeta) -> Option<()> {
    let mut stack = vec![Frame::Compound {
        root: true,
        data: false,
    }];
    loop {
        if stack.len() > MAX_DEPTH {
            return None;
        }
        let Some(top) = stack.last_mut() else {
            return Some(());
        };
        match top {
            Frame::Compound { root, data } => {
                let (root, data) = (*root, *data);
                let ty = r.u8()?;
                if ty == 0 {
                    stack.pop();
                    if data {
                        // Everything we want is inside `Data`.
                        return Some(());
                    }
                    continue;
                }
                let name = r.name()?;
                if root && ty == 10 && name == b"Data" {
                    stack.push(Frame::Compound {
                        root: false,
                        data: true,
                    });
                } else if data && ty == 8 && name == b"LevelName" {
                    let n = r.u16()?;
                    meta.level_name = Some(String::from_utf8_lossy(r.take(n)?).into_owned());
                } else if data && ty == 4 && name == b"LastPlayed" {
                    meta.last_played = Some(r.i64()?);
                } else {
                    value(r, ty, &mut stack)?;
                }
            }
            Frame::List { ty, left } => {
                if *left == 0 {
                    stack.pop();
                    continue;
                }
                *left -= 1;
                let ty = *ty;
                value(r, ty, &mut stack)?;
            }
        }
    }
}

/// Consumes one payload of type `ty`, pushing a frame for containers.
fn value(r: &mut Reader, ty: u8, stack: &mut Vec<Frame>) -> Option<()> {
    match ty {
        1 => drop(r.take(1)?),
        2 => drop(r.take(2)?),
        3 | 5 => drop(r.take(4)?),
        4 | 6 => drop(r.take(8)?),
        7 => {
            let n = r.len()?;
            r.take(n)?;
        }
        8 => {
            let n = r.u16()?;
            r.take(n)?;
        }
        9 => {
            let elem = r.u8()?;
            let n = r.len()?;
            if n > 0 {
                if elem == 0 || elem > 12 {
                    return None;
                }
                stack.push(Frame::List { ty: elem, left: n });
            }
        }
        10 => stack.push(Frame::Compound {
            root: false,
            data: false,
        }),
        11 => {
            let n = r.len()?;
            r.take(n.checked_mul(4)?)?;
        }
        12 => {
            let n = r.len()?;
            r.take(n.checked_mul(8)?)?;
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(out: &mut Vec<u8>, v: &str) {
        out.extend((v.len() as u16).to_be_bytes());
        out.extend(v.as_bytes());
    }

    /// Root{ Data{ LevelName, LastPlayed } } with `before` / `after` raw
    /// bytes spliced inside Data around the two fields.
    fn level(name: Option<&str>, last: Option<i64>, before: &[u8], after: &[u8]) -> Vec<u8> {
        let mut v = vec![10];
        s(&mut v, "");
        v.push(10);
        s(&mut v, "Data");
        v.extend(before);
        if let Some(n) = name {
            v.push(8);
            s(&mut v, "LevelName");
            s(&mut v, n);
        }
        if let Some(l) = last {
            v.push(4);
            s(&mut v, "LastPlayed");
            v.extend(l.to_be_bytes());
        }
        v.extend(after);
        v.extend([0, 0]);
        v
    }

    fn nested_compounds(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(10);
            s(&mut v, "x");
        }
        v
    }

    #[test]
    fn reads_both_fields_and_skips_all_tag_types() {
        let mut junk = Vec::new();
        for (ty, payload) in [
            (1u8, vec![1]),
            (2, vec![0, 1]),
            (3, vec![0; 4]),
            (5, vec![0; 4]),
            (6, vec![0; 8]),
            (7, vec![0, 0, 0, 2, 9, 9]),
            (8, vec![0, 1, b'z']),
            (11, vec![0, 0, 0, 1, 0, 0, 0, 5]),
            (12, vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 5]),
            // list of two empty compounds
            (9, vec![10, 0, 0, 0, 2, 0, 0]),
            // list of lists of ints
            (9, vec![9, 0, 0, 0, 1, 3, 0, 0, 0, 1, 0, 0, 0, 7]),
        ] {
            junk.push(ty);
            s(&mut junk, "k");
            junk.extend(payload);
        }
        let buf = level(Some("W"), Some(42), &junk, &[]);
        assert_eq!(
            read_level_meta(&buf),
            Some(LevelMeta {
                level_name: Some("W".into()),
                last_played: Some(42)
            })
        );
        // Fields after the junk, too.
        let buf = level(Some("W"), Some(42), &[], &junk);
        assert_eq!(read_level_meta(&buf).unwrap().last_played, Some(42));
    }

    #[test]
    fn deeply_nested_compounds_do_not_overflow() {
        let nest = nested_compounds(100_000);
        let buf = level(Some("Before"), None, &[], &nest);
        let m = read_level_meta(&buf).unwrap();
        assert_eq!(m.level_name.as_deref(), Some("Before"));
        let buf = level(None, None, &nest, &[]);
        assert_eq!(read_level_meta(&buf), None);
    }

    #[test]
    fn deeply_nested_lists_do_not_overflow() {
        // Only the first header names the list; the rest are list payloads.
        let mut v = vec![9u8];
        s(&mut v, "l");
        v.extend([9, 0, 0, 0, 1]);
        for _ in 0..100_000 {
            v.extend([9, 0, 0, 0, 1]);
        }
        let buf = level(Some("Before"), None, &[], &v);
        assert_eq!(
            read_level_meta(&buf).unwrap().level_name.as_deref(),
            Some("Before")
        );
        let buf = level(None, None, &v, &[]);
        assert_eq!(read_level_meta(&buf), None);
    }

    #[test]
    fn truncation_at_every_offset_never_panics() {
        let mut nest = nested_compounds(3);
        nest.extend([0, 0, 0]);
        let buf = level(Some("Name"), Some(5), &nest, &[]);
        for cut in 0..=buf.len() {
            let _ = read_level_meta(&buf[..cut]);
        }
    }

    #[test]
    fn hostile_lengths_do_not_allocate_or_panic() {
        // Huge declared int array / list / byte array lengths.
        for payload in [
            vec![11u8, 0, 1, b'k', 0x7f, 0xff, 0xff, 0xff],
            vec![12, 0, 1, b'k', 0x7f, 0xff, 0xff, 0xff],
            vec![7, 0, 1, b'k', 0xff, 0xff, 0xff, 0xff],
            vec![9, 0, 1, b'k', 10, 0x7f, 0xff, 0xff, 0xff],
            vec![9, 0, 1, b'k', 0, 0, 0, 0, 1],
            vec![99, 0, 1, b'k'],
        ] {
            let buf = level(None, None, &payload, &[]);
            assert_eq!(read_level_meta(&buf), None);
        }
    }

    #[test]
    fn invalid_utf8_is_lossy() {
        let mut buf = level(None, None, &[], &[]);
        let mut name = vec![8u8];
        s(&mut name, "LevelName");
        name.extend([0, 2, 0xff, b'a']);
        let at = buf.len() - 2;
        buf.splice(at..at, name);
        assert_eq!(
            read_level_meta(&buf).unwrap().level_name.as_deref(),
            Some("\u{fffd}a")
        );
    }
}
