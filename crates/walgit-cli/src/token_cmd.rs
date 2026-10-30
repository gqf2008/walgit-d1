//! `walgit token list|add|rm` — the host's static API tokens (`server.auth.tokens`).
//!
//! The tokens live in the same `walgit.toml` the next boot reads (D8 — one
//! config file, one auth story), so this command is the CLI's write half of a
//! file that the server only ever reads: it edits the document in place
//! through `toml_edit` (comments and ordering elsewhere in the file survive —
//! a serialize-from-struct round trip would drop both), validates the
//! **composed** document with `Config::parse_document` (TOML + `validate()`, no
//! `WALGIT__*` env: a file must stand on its own, the operator's shell is not
//! the server's), and only then replaces the file atomically through the same
//! 0600 temp-plus-rename path the setup wizard and the storage editor use
//! (D43/D44). A torn or invalid `walgit.toml` strands the instance, so the
//! write is last.
//!
//! One thing is *not* preserved: the file's two shapes for the list are
//! `tokens = [{…}]` and `[[server.auth.tokens]]`, and this command always writes
//! the inline array — so comments attached to individual `[[server.auth.tokens]]`
//! rows are lost when a row changes. Comments anywhere else in the file, and
//! comments inside an already-inline list, are untouched.
//!
//! What a token is: a `principal` (the name that appears in the WAL, in
//! `X-Walgit-Principal` and in collab entries), `write` (push; default true)
//! and `admin` (settings/policy/storage editor; default false). `wgt_…` access
//! tokens minted at `/_auth/tokens` are a different mechanism — stateless,
//! signed with `session_secret`, unlimited in number — and are not in this
//! file: this command cannot mint, list or revoke them.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};
use walgit_config::{AuthMode, Config};

use crate::TokenAction;

pub fn run(action: TokenAction, cfg: &Arc<Config>, config_path: &Path) -> Result<()> {
    match action {
        TokenAction::List => {
            list(cfg);
            Ok(())
        }
        TokenAction::Add {
            principal,
            token,
            token_env,
            token_stdin,
            write,
            no_write,
            admin,
            no_admin,
            force,
        } => {
            let secret = Secret::resolve(token, token_env, token_stdin)?;
            add(
                config_path,
                &principal,
                &secret,
                flag(write, no_write),
                flag(admin, no_admin),
                force,
                cfg.server.auth.mode,
            )
        }
        TokenAction::Rm { principal, force } => rm(config_path, &principal, force),
    }
}

/// `(positive, negative)` flag pair → `Some(true)` / `Some(false)` / `None`
/// when the operator said nothing. `None` is what lets `add --force` keep the
/// replaced row's permissions instead of silently resetting them.
fn flag(yes: bool, no: bool) -> Option<bool> {
    match (yes, no) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

/// Where the secret comes from. One of the three, never two.
enum Secret {
    Literal(String),
    FromEnv(String),
}

impl Secret {
    fn resolve(
        token: Option<String>,
        token_env: Option<String>,
        stdin: bool,
    ) -> Result<Self> {
        let given = usize::from(token.is_some()) + usize::from(token_env.is_some()) + usize::from(stdin);
        if given != 1 {
            bail!(
                "give exactly one of --token, --token-env, --token-stdin (got {given})\n\
                 --token-env NAME is the one to prefer: the secret stays out of the file, the\n\
                 shell history and `ps`"
            );
        }
        if let Some(name) = token_env {
            // An env var name, not a value: it must be readable back by the
            // server at start, so hold it to the portable shell shape.
            let mut chars = name.chars();
            let ok = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                bail!("--token-env must be an environment variable name, e.g. WALGIT_ALICE_TOKEN (got `{name}`)");
            }
            return Ok(Self::FromEnv(name));
        }
        let value = if stdin {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading the token from stdin")?;
            match text.lines().next() {
                Some(line) => line.trim().to_string(),
                None => String::new(),
            }
        } else {
            token.unwrap_or_default().trim().to_string()
        };
        if value.is_empty() {
            bail!("the token is empty");
        }
        if value.chars().any(char::is_whitespace) {
            bail!("the token contains whitespace; a bearer token is one opaque word");
        }
        if value.starts_with("Bearer ") {
            bail!("the token must be the credential itself, not `Bearer …` — the client adds the scheme");
        }
        Ok(Self::Literal(value))
    }

    fn describe(&self) -> String {
        match self {
            Self::Literal(_) => "literal in walgit.toml".to_string(),
            Self::FromEnv(name) => match std::env::var(name) {
                Ok(_) => format!("env {name}"),
                Err(_) => format!("env {name} (unset in this shell)"),
            },
        }
    }

    fn row(&self, principal: &str, write: bool, admin: bool) -> InlineTable {
        let mut t = InlineTable::new();
        t.insert("principal", Value::from(principal));
        match self {
            Self::Literal(v) => {
                t.insert("token", Value::from(v.as_str()));
            }
            Self::FromEnv(name) => {
                t.insert("token_env", Value::from(name.as_str()));
            }
        }
        // Defaults are omitted so the file stays readable: write is true unless
        // the operator said --no-write, admin is false unless --admin.
        if !write {
            t.insert("write", Value::from(false));
        }
        if admin {
            t.insert("admin", Value::from(true));
        }
        t
    }
}

fn list(cfg: &Config) {
    let auth = &cfg.server.auth;
    println!("mode = {}", mode_name(auth.mode));
    if auth.tokens.is_empty() {
        println!("no static tokens: everyone is anonymous here");
        hint_for_mode(auth.mode);
        return;
    }
    println!("{:<24} {:>5} {:>5}  SECRET", "PRINCIPAL", "WRITE", "ADMIN");
    for t in &auth.tokens {
        // `token_env` wins over a literal when both are set (`auth.rs`
        // `resolve_tokens`), so the listing must not claim the literal is in use.
        let secret = match (&t.token_env, t.token.is_empty()) {
            (Some(name), _) if !name.is_empty() => match std::env::var(name) {
                Ok(_) if t.token.is_empty() => format!("env {name}"),
                Ok(_) => format!("env {name} (overrides the literal in walgit.toml)"),
                Err(_) if t.token.is_empty() => format!("env {name} (unset in this shell)"),
                Err(_) => format!("env {name} (unset here; the literal fallback is in use)"),
            },
            _ if t.token.is_empty() => "MISSING (no token, no token_env)".to_string(),
            _ => "literal in walgit.toml".to_string(),
        };
        println!(
            "{:<24} {:>5} {:>5}  {}",
            t.principal,
            yes(t.write),
            yes(t.admin),
            secret
        );
    }
    hint_for_mode(auth.mode);
}

fn hint_for_mode(mode: AuthMode) {
    match mode {
        AuthMode::None => eprintln!(
            "note: server.auth.mode = \"none\" — every request is already `anon` with write and\n\
             admin, so these tokens are not consulted. Set mode = \"token\" (or \"oidc\") first."
        ),
        AuthMode::Token | AuthMode::Oidc => {}
    }
}

fn add(
    config_path: &Path,
    principal: &str,
    secret: &Secret,
    write: Option<bool>,
    admin: Option<bool>,
    force: bool,
    mode: AuthMode,
) -> Result<()> {
    check_principal(principal)?;
    // `anon` is what an unauthenticated request is called (auth.rs); a token
    // authenticating as `anon` would be indistinguishable from "no credential"
    // in every log line, policy rule and collab entry it touches.
    if principal == "anon" {
        bail!("`anon` is the name of an unauthenticated request; pick a real principal name");
    }
    let mut replaced: Option<(bool, bool)> = None;
    let mut write_out = write.unwrap_or(true);
    let mut admin_out = admin.unwrap_or(false);
    save_rows(config_path, SAVE_ADD, |rows| {
        // A repeated *value* is never consulted (first match wins), so refuse it
        // here rather than let a dead row into the file. The file validator
        // says the same thing on the next boot.
        if let Secret::Literal(v) = secret
            && let Some(other) = rows.iter().find(|r| {
                principal_of(r) != Some(principal)
                    && r.get("token").and_then(Value::as_str) == Some(v.as_str())
            })
        {
            bail!(
                "that token value is already in use by {:?}; give `{principal}` its own",
                principal_of(other).unwrap_or("another principal")
            );
        }
        match rows.iter().rposition(|r| principal_of(r) == Some(principal)) {
            Some(_) if !force => bail!(
                "`{principal}` already has a token in {}; pass --force to replace it,\n\
                 or remove it first with `walgit token rm {principal}`",
                config_path.display()
            ),
            Some(i) => {
                // A replacement changes the *secret*; permissions stay as they
                // were unless the operator said otherwise, so rotating a
                // read-only token cannot silently hand it write.
                let old = rows.get(i).map(|r| (permission(r, "write", true), permission(r, "admin", false)));
                if let Some((w, a)) = old {
                    replaced = Some((w, a));
                    write_out = write.unwrap_or(w);
                    admin_out = admin.unwrap_or(a);
                }
                let row = secret.row(principal, write_out, admin_out);
                rows.retain(|r| principal_of(r) != Some(principal));
                rows.insert(i.min(rows.len()), row);
                Ok(1)
            }
            None => {
                rows.push(secret.row(principal, write_out, admin_out));
                Ok(1)
            }
        }
    })?;
    let kept = match replaced {
        Some(_) if write.is_none() || admin.is_none() => " (permissions kept from the replaced row)",
        _ => "",
    };
    println!(
        "added token for `{principal}` (write {}, admin {}, {}) to {}{kept}",
        yes(write_out),
        yes(admin_out),
        secret.describe(),
        config_path.display()
    );
    hint_for_mode(mode);
    restart_note();
    Ok(())
}

/// A permission column of a token row, defaulted the way the server defaults it.
fn permission(row: &InlineTable, key: &str, default: bool) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn rm(config_path: &Path, principal: &str, force: bool) -> Result<()> {
    let removed = save_rows(config_path, SAVE_REMOVE, |rows| {
        let before = rows.len();
        rows.retain(|r| principal_of(r) != Some(principal));
        let removed = before - rows.len();
        if removed == 0 && !force {
            bail!(
                "no token for `{principal}` in {} (run `walgit token list`)",
                config_path.display()
            );
        }
        Ok(removed)
    })?;
    if removed == 0 {
        println!("no token for `{principal}` — nothing to do");
    } else {
        println!(
            "removed {removed} token(s) for `{principal}` from {}",
            config_path.display()
        );
        restart_note();
    }
    Ok(())
}

const SAVE_ADD: &str = "add";
const SAVE_REMOVE: &str = "remove";

/// The one write path of both commands: read the file, let `edit` change the
/// token rows, validate the composed document the way the next boot will, and
/// only then replace the file atomically. Returns whatever `edit` returned.
fn save_rows<F>(config_path: &Path, what: &str, edit: F) -> Result<usize>
where
    F: FnOnce(&mut Vec<InlineTable>) -> Result<usize>,
{
    // A `--config /dev/null` instance (D39: "defaults + env") has nowhere to
    // write: renaming a temp over the device would replace it. Same rule as the
    // storage editor's `can_save` (D44).
    let meta = std::fs::metadata(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    if !meta.is_file() {
        bail!(
            "{} is not a regular file — `walgit token {what}` edits a real walgit.toml \
             (a deployment whose config is only env vars has no token list to edit)",
            config_path.display()
        );
    }
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", config_path.display()))?;
    let mut rows = token_rows(&doc)?;
    let changed = edit(&mut rows)?;
    if changed == 0 {
        return Ok(0);
    }
    set_tokens(&mut doc, rows)?;
    let composed = doc.to_string();
    // Validate first, write second: a file that would not boot is never
    // installed. `parse_document` is the file on its own — TOML, then
    // `validate()` — deliberately **without** this shell's `WALGIT__*` env, so
    // an environment variable cannot bless a file that would not boot on a host
    // without it.
    Config::parse_document(&composed).map_err(|e| {
        let detail = format!("{e:#}");
        if detail.contains("at least one token") {
            anyhow::anyhow!(
                "{} would be left with no token while `server.auth.mode = \"token\"`, which \
                 locks every client out. Add the replacement first, set \
                 `server.auth.mode = \"none\"`/`\"oidc\"`, or write the file by hand. Nothing \
                 was written ({detail})",
                config_path.display()
            )
        } else {
            anyhow::anyhow!(
                "the edited {} would not start; nothing was written. Cause: {detail}",
                config_path.display()
            )
        }
    })?;
    if composed == text {
        // The edit asked for what the file already says (e.g. `--force`
        // re-creating an identical row): say so instead of reporting a write.
        eprintln!(
            "note: {} already said exactly that, so the file was left unchanged",
            config_path.display()
        );
        return Ok(changed);
    }
    walgit_server::setup_wizard::write_atomic(config_path, &composed)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", config_path.display()))?;
    Ok(changed)
}

fn restart_note() {
    println!(
        "the server reads tokens at start: restart it (`walgit service restart`) for this to take effect"
    );
}

/// Every token table in `server.auth.tokens`, whichever of the two TOML shapes
/// the file uses (`tokens = [{…}]` or `[[server.auth.tokens]]`).
fn token_rows(doc: &DocumentMut) -> Result<Vec<InlineTable>> {
    let Some(item) = doc
        .get("server")
        .and_then(Item::as_table_like)
        .and_then(|server| server.get("auth"))
        .and_then(Item::as_table_like)
        .and_then(|auth| auth.get("tokens"))
    else {
        return Ok(Vec::new());
    };
    match item {
        Item::Value(Value::Array(arr)) => arr
            .iter()
            .map(|v| match v {
                Value::InlineTable(t) => Ok(t.clone()),
                _ => bail!("server.auth.tokens holds a non-table entry; every token is a table"),
            })
            .collect(),
        Item::ArrayOfTables(aot) => Ok(aot.iter().map(|t| t.clone().into_inline_table()).collect()),
        _ => bail!("server.auth.tokens is not an array of tables"),
    }
}

/// Replace `server.auth.tokens` with `rows` (an empty list removes the key —
/// an empty `tokens = []` would still read as "token mode with no token").
fn set_tokens(doc: &mut DocumentMut, rows: Vec<InlineTable>) -> Result<()> {
    let server = doc
        .entry("server")
        .or_insert(Item::Table(Table::new()));
    let server = server
        .as_table_like_mut()
        .context("`server` in walgit.toml is not a table")?;
    let auth = server.entry("auth").or_insert(Item::Table(Table::new()));
    let auth = auth
        .as_table_like_mut()
        .context("`server.auth` in walgit.toml is not a table")?;
    if rows.is_empty() {
        auth.remove("tokens");
        return Ok(());
    }
    let mut arr = Array::new();
    for row in rows {
        arr.push(Value::InlineTable(row));
    }
    auth.insert("tokens", Item::Value(Value::Array(arr)));
    Ok(())
}

fn principal_of(row: &InlineTable) -> Option<&str> {
    row.get("principal").and_then(Value::as_str)
}

/// A principal name travels into URLs, log lines, `X-Walgit-Principal`, ref
/// paths (`refs/collab/inbox/<actor>/…`), `<principal>.ed25519` key file names
/// and the MCP write gate, so it is held to **the** ref-segment rule the server
/// applies (`walgit_server::web::api::ref_segment_ok`) rather than a looser copy
/// of it — a name this accepts is a name every one of those accepts.
fn check_principal(principal: &str) -> Result<()> {
    if principal.is_empty() {
        bail!("the principal name is empty");
    }
    if principal.len() > 128 {
        bail!("the principal name is longer than 128 bytes");
    }
    if !walgit_server::web::api::ref_segment_ok(principal) {
        bail!(
            "`{principal}`: a principal name is ASCII letters, digits, `-`, `_`, `.` and `@`, \
             starting with a letter or digit, without `..` and without a `.lock` suffix \
             (it becomes a ref path segment and a key file name)"
        );
    }
    Ok(())
}

fn yes(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

fn mode_name(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::None => "none",
        AuthMode::Token => "token",
        AuthMode::Oidc => "oidc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    #[test]
    fn reads_both_shapes_of_the_token_list() {
        let inline = doc("[server.auth]\ntokens = [{ principal = \"a\", token = \"t\" }]\n");
        assert_eq!(token_rows(&inline).unwrap().len(), 1);
        let aot = doc("[[server.auth.tokens]]\nprincipal = \"a\"\ntoken = \"t\"\n");
        assert_eq!(token_rows(&aot).unwrap().len(), 1);
        let none = doc("[server]\nlisten = \"127.0.0.1:8080\"\n");
        assert!(token_rows(&none).unwrap().is_empty());
    }

    #[test]
    fn writing_creates_the_section_and_keeps_the_rest_of_the_file() {
        let mut d = doc("# a comment the operator wrote\n[server]\nlisten = \"127.0.0.1:8080\"\n");
        let secret = Secret::Literal("s3cret".to_string());
        set_tokens(&mut d, vec![secret.row("alice", false, true)]).unwrap();
        let out = d.to_string();
        assert!(out.contains("# a comment the operator wrote"));
        assert!(out.contains("listen = \"127.0.0.1:8080\""));
        assert!(out.contains("principal = \"alice\""));
        assert!(out.contains("write = false"));
        assert!(out.contains("admin = true"));
        // it must still be the config the server boots from
        Config::parse(&out).unwrap();
    }

    #[test]
    fn removing_the_last_token_drops_the_key() {
        let mut d = doc("[server.auth]\ntokens = [{ principal = \"a\", token = \"t\" }]\n");
        set_tokens(&mut d, Vec::new()).unwrap();
        assert!(!d.to_string().contains("tokens"));
    }

    #[test]
    fn a_secret_is_one_word_and_never_the_bearer_prefix() {
        assert!(Secret::resolve(Some("Bearer x".to_string()), None, false).is_err());
        assert!(Secret::resolve(Some("two words".to_string()), None, false).is_err());
        assert!(Secret::resolve(Some("  ".to_string()), None, false).is_err());
        assert!(Secret::resolve(Some("ok".to_string()), Some("X".to_string()), false).is_err());
        assert!(Secret::resolve(None, None, false).is_err());
        assert!(Secret::resolve(None, Some("WALGIT_TOKEN".to_string()), false).is_ok());
    }

    #[test]
    fn an_edit_that_would_not_boot_changes_nothing() {
        // The whole reason the write is last: a token-mode file with an entry
        // that carries neither `token` nor `token_env` does not validate, so the
        // edit must be refused and the file left exactly as it was.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        let before = "\n[server]\nlisten = \"127.0.0.1:8080\"\n\n[server.auth]\nmode = \"token\"\n\n\
                      [store]\nbackend = \"memory\"\nmemory_backend_intentional = true\n";
        std::fs::write(&path, before).unwrap();
        let err = save_rows(&path, SAVE_ADD, |rows| {
            let mut t = InlineTable::new();
            t.insert("principal", Value::from("nobody"));
            rows.push(t);
            Ok(1)
        })
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("would not start"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// A token-mode config whose `[server.auth]` already holds `tokens`
    /// (an empty string for none).
    fn token_mode_config(tokens: &str) -> String {
        format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[server.auth]\nmode = \"token\"\n{tokens}\n\n\
             [store]\nbackend = \"memory\"\nmemory_backend_intentional = true\n"
        )
    }

    /// `add` → the file, not just `set_tokens` in memory: the ordering
    /// (read → edit → validate → write) is what the command is.
    #[test]
    fn adding_a_token_writes_a_file_the_server_can_boot_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        std::fs::write(&path, token_mode_config("")).unwrap();
        add(
            &path,
            "alice",
            &Secret::Literal("a1".to_string()),
            None,
            Some(true),
            false,
            AuthMode::Token,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        let t = &cfg.server.auth.tokens[0];
        assert_eq!(t.principal, "alice");
        assert_eq!(t.token, "a1");
        assert!(t.write, "write defaults on");
        assert!(t.admin);
    }

    /// A rotation changes the secret, not the permissions: `--force` without a
    /// permission flag must not hand a read-only token the write bit.
    #[test]
    fn a_replacement_keeps_the_permissions_it_did_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        std::fs::write(
            &path,
            token_mode_config(
                "tokens = [{ principal = \"alice\", token = \"old\", write = false }]"
            ),
        )
        .unwrap();
        add(
            &path,
            "alice",
            &Secret::Literal("new".to_string()),
            None,
            None,
            true,
            AuthMode::Token,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.auth.tokens[0].token, "new");
        assert!(!cfg.server.auth.tokens[0].write, "permissions were kept");
    }

    #[test]
    fn a_repeated_token_value_is_refused_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        let before =
            token_mode_config("tokens = [{ principal = \"alice\", token = \"same\" }]");
        std::fs::write(&path, &before).unwrap();
        let err = add(
            &path,
            "bob",
            &Secret::Literal("same".to_string()),
            None,
            None,
            false,
            AuthMode::Token,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already in use by \"alice\""), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// The most likely `rm` is the last one, and it would lock every client out.
    #[test]
    fn removing_the_last_token_names_the_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        std::fs::write(
            &path,
            token_mode_config("tokens = [{ principal = \"alice\", token = \"a1\" }]"),
        )
        .unwrap();
        let err = rm(&path, "alice", false).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("locks every client out"), "{text}");
    }

    #[test]
    fn a_principal_name_is_a_ref_segment() {
        assert!(check_principal("alice").is_ok());
        assert!(check_principal("ci-robot.1@example").is_ok());
        assert!(check_principal("").is_err());
        assert!(check_principal("has space").is_err());
        assert!(check_principal("refs/heads/x").is_err());
    }
}
