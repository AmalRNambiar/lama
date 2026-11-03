//! The system clipboard: text selected in a console is copied out, and the
//! clipboard can be pasted into a console's prompt.
//!
//! Copying goes through the terminal (OSC 52), so it works wherever lama's
//! terminal does — over ssh too. kitty allows clipboard writes by default
//! (`clipboard_control`). Reading back asks the desktop (`wl-paste`, else
//! `xclip`, or `pbpaste` on macOS); a terminal's own paste (ctrl+shift+v) arrives as a bracketed
//! paste instead and needs neither.

use std::io::Write;
use std::process::{Command, Stdio};

/// Put `text` on the clipboard and the primary selection.
pub fn copy(text: &str) -> std::io::Result<()> {
    let data = base64(text.as_bytes());
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{data}\x07\x1b]52;p;{data}\x07")?;
    out.flush()
}

/// The clipboard's text, if the desktop hands it over.
pub fn paste() -> Option<String> {
    let tries: [&[&str]; 3] = [
        &["wl-paste", "--no-newline", "--type", "text"],
        &["xclip", "-o", "-selection", "clipboard"],
        &["pbpaste"],
    ];
    tries.iter().find_map(|argv| {
        let out = Command::new(argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            s.push(if i <= chunk.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo ✓".as_bytes()), "aMOpbGxvIOKckw==");
    }
}
