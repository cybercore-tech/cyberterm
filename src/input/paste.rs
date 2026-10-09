// src/input/paste.rs
//
// Turns clipboard text into the bytes written to the PTY.

/// With bracketed paste on (mode 2004), the text is wrapped in
/// `ESC[200~ ... ESC[201~` so the shell or editor knows it was pasted, not
/// typed. Every ESC inside the text is dropped first: otherwise a crafted
/// clipboard containing `ESC[201~` could end the bracket early and have the
/// rest run as if typed (the classic paste-jacking attack).
///
/// Without bracketed paste, newlines become carriage returns -- the byte the
/// Enter key sends -- which is what programs reading the raw PTY expect.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend(text.bytes().filter(|&b| b != 0x1b));
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_and_strips_escapes() {
        assert_eq!(
            paste_bytes("ls\x1b[201~rm -rf ~\n", true),
            b"\x1b[200~ls[201~rm -rf ~\n\x1b[201~".to_vec()
        );
    }

    #[test]
    fn plain_paste_converts_newlines_to_carriage_returns() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }
}
