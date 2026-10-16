//! Classic uuencode/uudecode — the 7-bit-safe envelope for moving a tar over
//! the telnet exec channel. The tty line discipline mangles arbitrary binary
//! (control chars, CR/LF, ^C/^D), but uuencode's alphabet is all printable, so
//! it rides through unharmed. We encode/decode in Rust so the host side needs
//! no `sharutils`; the guest uses its stock `uuencode`/`uudecode`.

/// Encode one byte's low 6 bits. Historic uuencode maps 0 to backtick (not
/// space) so trailing zeros can't be eaten as trailing whitespace.
fn enc(v: u8) -> u8 {
    let v = v & 0x3f;
    if v == 0 { b'`' } else { v + 0x20 }
}

fn dec(c: u8) -> u8 {
    if c == b'`' { 0 } else { c.wrapping_sub(0x20) & 0x3f }
}

/// Produce a `begin 644 <name> … end` uuencoded stream for `data`.
pub fn encode(name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("begin 644 {name}\n").into_bytes();
    for chunk in data.chunks(45) {
        out.push(enc(chunk.len() as u8));
        for grp in chunk.chunks(3) {
            let b0 = grp[0];
            let b1 = grp.get(1).copied().unwrap_or(0);
            let b2 = grp.get(2).copied().unwrap_or(0);
            out.push(enc(b0 >> 2));
            out.push(enc((b0 << 4) | (b1 >> 4)));
            out.push(enc((b1 << 2) | (b2 >> 6)));
            out.push(enc(b2));
        }
        out.push(b'\n');
    }
    out.push(b'`'); // zero-length line
    out.push(b'\n');
    out.extend_from_slice(b"end\n");
    out
}

/// Decode a uuencoded stream, tolerating CRs and the `begin`/`end` frame.
pub fn decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for raw in input.split(|&b| b == b'\n') {
        // Strip a trailing CR.
        let line = match raw.last() {
            Some(&b'\r') => &raw[..raw.len() - 1],
            _ => raw,
        };
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"begin ") {
            continue;
        }
        if line == b"end" {
            break;
        }
        let count = dec(line[0]) as usize;
        if count == 0 {
            continue;
        }
        let body = &line[1..];
        let mut produced = 0;
        let mut i = 0;
        while produced < count && i + 1 < body.len() {
            let v0 = dec(body[i]);
            let v1 = dec(body[i + 1]);
            let v2 = if i + 2 < body.len() { dec(body[i + 2]) } else { 0 };
            let v3 = if i + 3 < body.len() { dec(body[i + 3]) } else { 0 };
            let bytes = [(v0 << 2) | (v1 >> 4), (v1 << 4) | (v2 >> 2), (v2 << 6) | v3];
            for &b in &bytes {
                if produced < count {
                    out.push(b);
                    produced += 1;
                }
            }
            i += 4;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for data in [
            &b""[..],
            &b"a"[..],
            &b"ab"[..],
            &b"abc"[..],
            &b"hello, world\n"[..],
            &(0u8..=255).collect::<Vec<u8>>()[..],
        ] {
            let encoded = encode("x", data);
            let decoded = decode(&encoded);
            assert_eq!(decoded, data, "round trip failed for {} bytes", data.len());
        }
    }

    #[test]
    fn tolerates_crlf() {
        let data = (0u8..200).collect::<Vec<u8>>();
        let encoded = encode("x", &data);
        let crlf = String::from_utf8(encoded).unwrap().replace('\n', "\r\n").into_bytes();
        assert_eq!(decode(&crlf), data);
    }

    #[test]
    fn truncated_input_decodes_what_arrived() {
        let data = (0u8..90).collect::<Vec<u8>>();
        let encoded = String::from_utf8(encode("x", &data)).unwrap();
        // No `end` (and no zero-length line): both full lines still decode.
        let unended = encoded.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(decode(unended.as_bytes()), data);
        // A line cut on a group boundary yields the bytes of its whole groups.
        let line = encoded.lines().nth(1).unwrap();
        let cut = format!("{}\n", &line[..21]);
        assert_eq!(decode(cut.as_bytes()), data[..15]);
    }

    #[test]
    fn garbage_lines_do_not_panic() {
        let junk: &[&[u8]] = &[
            b"!",
            b"M",
            b"\xff\xfe\x00\x01",
            b"Mxx",
            b"rix: no such dir in guest: /workarea\n",
            &[b'M'; 200],
        ];
        for line in junk {
            // A line's length byte promises at most 63 bytes.
            assert!(decode(line).len() <= 63, "{line:?}");
        }
        // Lines after `end` are ignored.
        assert_eq!(decode(b"begin 644 x\n#86)C\n`\nend\n#86)C\n"), b"abc");
    }
}
