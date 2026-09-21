//! `walgit-service-host` — the windowless half of the Windows service task.
//!
//! The Task Scheduler's `Exec` action **cannot** create a process with
//! `CREATE_NO_WINDOW`; it always gives the action a console. On Windows 11 (and
//! on any Windows 10 where Windows Terminal is the default terminal) that
//! console is handed to Windows Terminal, which puts a window on screen (or at
//! least a taskbar button) — the "black box" D48 was supposed to remove.
//! `powershell -WindowStyle Hidden` only got that window hidden *initially*:
//! WT created it minimised, so clicking the taskbar entry brought the console
//! right back (measured on a real machine 2026-09-19; thread
//! `cc-ai-win-service-console`).
//!
//! So the action is this binary instead: a **GUI-subsystem** process built from
//! this file alone — no dependency on the rest of the CLI, so the launcher stays
//! a few hundred kilobytes — which runs the task's command through
//! `cmd /d /s /c` with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP |
//! DETACHED_PROCESS` and waits for it. Nothing in the tree ever gets a console;
//! `cmd` still owns the `>> server.log 2>&1` redirect (that is what keeps
//! "append, never truncate"), and the task still tracks the server's lifetime
//! because this process stays alive until the server exits.
//!
//! The transport is the same `-EncodedCommand <base64 of UTF-16LE>` the
//! PowerShell wrapper used, so paths with spaces and quotes never have to
//! survive a second round of command-line parsing.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    std::process::exit(main_entry());
}

/// Returns the child's exit code, or a non-zero code when the command could not
/// be decoded or started. Nothing is printed: a GUI-subsystem process has
/// nowhere to print, and the task's `LastTaskResult` is the record.
fn main_entry() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(encoded) = encoded_command(&args) else {
        return 2;
    };
    let Some(command) = decode_command(&encoded) else {
        return 2;
    };
    // A PowerShell-written payload may carry a BOM; strip it so the first token
    // is really the program.
    run_command(command.trim_start_matches('\u{feff}'))
}

/// The argument after `-EncodedCommand` (the flag is case-insensitive and its
/// value may be quoted). `None` when the flag or its value is missing.
fn encoded_command(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg.eq_ignore_ascii_case("-EncodedCommand") {
            return it
                .next()
                .map(|value| value.trim_matches('"').to_string())
                .filter(|value| !value.is_empty());
        }
    }
    None
}

/// base64 (UTF-16LE) → the command `cmd /c` receives.
fn decode_command(encoded: &str) -> Option<String> {
    let bytes = decode_base64(encoded)?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16(&units).ok()
}

/// Standard base64 with `=` padding. Hand-rolled so the launcher depends on
/// nothing: it runs on the machine's boot path, before anything else.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    // A group of exactly one base64 character encodes nothing: reject the input
    // rather than silently returning an empty payload that would run nothing.
    let significant = input
        .bytes()
        .filter(|byte| !matches!(byte, b'\r' | b'\n' | b' ' | b'\t' | b'='))
        .count();
    if significant % 4 == 1 {
        return None;
    }
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xFF).unwrap_or_default());
        }
    }
    Some(out)
}

/// `CREATE_NO_WINDOW` — the scheduler cannot pass it, this process can.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// The server gets its own process group, exactly like the pre-D48 tray spawn.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// `cmd /d /s /c <command>`, created without a console, waited for.
#[cfg(windows)]
fn run_command(command: &str) -> i32 {
    use std::os::windows::process::CommandExt;

    let comspec = std::env::var_os("ComSpec")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("cmd.exe"));
    let mut cmd = std::process::Command::new(comspec);
    cmd.args(["/d", "/s", "/c"]);
    // One **raw** argument: `command` already carries the `""…""` quoting the `/s`
    // rule expects and the `>> log 2>&1` redirect, and must not be re-quoted by
    // the std layer (which would break the nested quotes).
    cmd.raw_arg(command);
    // **No** `DETACHED_PROCESS` here, deliberately. A detached process has no
    // console at all, so `cmd` would hand none down and the console-subsystem
    // server would allocate a *fresh* one — and on a machine whose default
    // terminal is Windows Terminal, that new console is a window again (measured:
    // the tree gained a `conhost.exe` under `walgit.exe` and a WT window with it).
    // `CREATE_NO_WINDOW` instead gives `cmd` a console *without a window*, which
    // every child inherits: no window anywhere, and `cmd` keeps owning the
    // `>> server.log 2>&1` redirect.
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    // No console means no meaningful std handles: hand the child nothing rather
    // than an inheritable handle a GUI process does not have.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    match cmd.spawn().and_then(|mut child| child.wait()) {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 3,
    }
}

#[cfg(not(windows))]
fn run_command(_command: &str) -> i32 {
    // The task and this transport are Windows-only; on other platforms the binary
    // exists only so the workspace builds everywhere.
    0
}

#[cfg(test)]
mod tests {
    use super::{decode_command, encoded_command};

    /// The encoder the CLI uses (`base64::engine::general_purpose::STANDARD` over
    /// UTF-16LE). Hand-rolled so the launcher needs no dependency at all.
    fn encode(text: &str) -> String {
        const ALPHABET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut utf16 = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let mut out = String::new();
        for chunk in utf16.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn the_flag_is_found_however_it_is_cased_or_quoted() {
        let args = vec![
            "-NoLogo".to_string(),
            "-encodedcommand".to_string(),
            "\"AQID\"".to_string(),
        ];
        assert_eq!(encoded_command(&args).as_deref(), Some("AQID"));
        assert_eq!(encoded_command(&["-EncodedCommand".to_string()]), None);
        assert_eq!(encoded_command(&[]), None);
    }

    #[test]
    fn the_payload_round_trips_as_utf16le() {
        let line = r#"""C:\wal git\walgit.exe" serve --config "C:\u\walgit.toml" >> "C:\u\server.log" 2>&1""#;
        assert_eq!(decode_command(&encode(line)).as_deref(), Some(line));
        // A BOM is tolerated: PowerShell writes one on some paths.
        assert_eq!(
            decode_command(&encode("\u{feff}cmd /c x")).as_deref(),
            Some("\u{feff}cmd /c x")
        );
    }

    #[test]
    fn a_malformed_payload_is_rejected_rather_than_guessed() {
        assert_eq!(decode_command("!!!!"), None);
        assert_eq!(decode_command("A"), None, "an odd byte count is not UTF-16");
    }

    /// The action must run the server through `cmd` with no console: `cmd` is
    /// what turns `>> server.log 2>&1` into an append, and the creation flags are
    /// what keep Windows from handing the task a console it would display.
    #[cfg(windows)]
    #[test]
    fn the_command_is_handed_to_cmd_verbatim() {
        use std::os::windows::process::CommandExt;

        let command = r#"""C:\wal git\walgit.exe" serve --config "C:\u\walgit.toml" >> "C:\u\server.log" 2>&1""#;
        let comspec = std::env::var_os("ComSpec")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsString::from("cmd.exe"));
        let mut cmd = std::process::Command::new(&comspec);
        cmd.args(["/d", "/s", "/c"]);
        cmd.raw_arg(command);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args.len(), 4);
        assert_eq!(args[3], command, "the redirect and the nested quotes reach cmd untouched");
    }
}
