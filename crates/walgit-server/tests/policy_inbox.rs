//! Per-actor collab-inbox isolation (issue #75 ⑤b, token mode): a principal
//! may push only its own `refs/collab/inbox/<actor>/*` — a second identity is
//! refused by the policy rule (the gate reads the **transport identity** the
//! token resolves to, not the entry signature), and clearing the policy rolls
//! the behavior back. Mirrors the manual matrix in issue #75 comment
//! 5548440019 as a repeatable test.

mod harness;

use harness::{Server, TestRepo, git_in};
use std::process::Command;
type TestResult = anyhow::Result<()>;

async fn start_server_with_two_principals() -> anyhow::Result<Server> {
    // A fresh loopback listener, so `token` mode validates. Two principals,
    // both with write + admin (admin is irrelevant here; the gate is policy).
    Server::start_with_tweak(|cfg| {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "alice".into(),
                token: "alice-s3cret".into(),
                token_env: None,
                write: true,
                admin: true,
            },
            walgit_config::StaticToken {
                principal: "bob".into(),
                token: "bob-s3cret".into(),
                token_env: None,
                write: true,
                admin: true,
            },
        ];
    })
    .await
}

fn inbox_policy() -> String {
    r#"{
  "version": 1,
  "rules": [
    {
      "name": "lock-alice-inbox",
      "match": { "refs": ["refs/collab/inbox/alice/*"] },
      "effect": {
        "protect": { "restricts": ["create", "update", "delete"], "bypass": ["alice"] }
      }
    },
    {
      "name": "lock-bob-inbox",
      "match": { "refs": ["refs/collab/inbox/bob/*"] },
      "effect": { "protect": { "restricts": ["create", "update", "delete"], "bypass": ["bob"] }
      }
    }
  ]
}"#
    .to_string()
}

/// Redact everything in `s` that could carry this test's credential.
///
/// Vectors, checked against real `git 2.50.1` output (issue #142's review):
///
/// - the **`== Info: Issue another request to this URL: 'http://git:<token>@host/…'`**
///   trace line — the one that still prints the credential in the clear, and it names no
///   header, so only the literal replacement catches it;
/// - an **`Authorization:` header line** — current git redacts it itself
///   (`Basic <redacted>`), older gits printed the base64 of `git:<token>`, which no literal
///   replacement can catch, so the whole line goes;
/// - a **URL userinfo echo** in `unable to access '…'` — git ≥ 2.32 masks that itself; the
///   literal replacement is the belt for older clients.
///
/// Precondition: the harness percent-encodes the password
/// (`Server::repo_url_with_userinfo`), so a token containing characters that encode
/// differently (e.g. `+ / =`) would appear in URLs in its encoded form and slip past the
/// literal replacement — the fixture tokens here use only unreserved characters. Keep it so,
/// or extend this function to scrub the encoded form too.
///
/// The returned string is what feeds `assert!(…, "{err}")` messages, and those land in CI
/// logs on failure. The token here is a fixture, not a secret — the point is that the shape
/// never becomes how this suite reports failures.
fn scrub_credentials(s: &str, token: &str) -> String {
    let replaced = s.replace(token, "<redacted>");
    replaced
        .lines()
        .filter(|l| !l.to_ascii_lowercase().contains("authorization:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Push `HEAD` as `refs/collab/inbox/<actor>/<name>` from `src` with the
/// transport identity of `token`. Returns success (the server accepted) and
/// **credential-scrubbed** stderr ([`scrub_credentials`]).
fn push_inbox(src: &TestRepo, server: &Server, actor: &str, name: &str, token: &str) -> (bool, String) {
    let ref_name = format!("refs/collab/inbox/{actor}/{name}");
    // An orphan commit per push: the inbox is append-only refs, unrelated
    // history between them is the normal shape.
    let base = format!("work-{actor}-{name}");
    let mut ok = git_in(src, &["checkout", "--orphan", &base]).is_ok();
    ok &= git_in(
        src,
        &[
            "-c",
            "user.name=tester",
            "-c",
            "user.email=t@walgit",
            "commit",
            "--allow-empty",
            "-m",
            name,
        ],
    )
    .is_ok();
    if !ok {
        return (false, String::new());
    }
    // URL userinfo form — the #79 challenge flow (§1.3); password = token.
    // No GIT_TRACE_CURL here: it printed the request-header set into the stderr that feeds
    // every assert message in this file (issue #142). `env_remove` too, so a runner or a
    // developer shell exporting it globally cannot resurrect the trace.
    let out = Command::new("git")
        .current_dir(&src.dir)
        .env_remove("GIT_TRACE_CURL")
        .env_remove("GIT_CURL_VERBOSE")
        .args([
            "push",
            &server.repo_url_with_userinfo("t", "secured", "git", token),
            &format!("HEAD:{ref_name}"),
        ])
        .output();
    match out {
        Ok(o) => {
            let err = scrub_credentials(&String::from_utf8_lossy(&o.stderr), token);
            // Wiring guard: the last line before the value escapes into `assert!(…, "{err}")`
            // messages. It states the invariant the unit test cannot reach — *whatever git
            // printed, what this returns carries no credential* — so a future trace vector
            // (or a scrub weakened to miss one) fails here, on every push in the suite,
            // rather than appearing in a CI log. It does NOT fire on the scrub call alone
            // being deleted while git stays silent: that combination leaks nothing today.
            assert!(
                !err.contains(token) && !err.to_ascii_lowercase().contains("authorization:"),
                "push_inbox is returning unscrubbed stderr (#142)"
            );
            (o.status.success(), err)
        }
        Err(e) => (false, format!("{e}")),
    }
}

/// One principal-relative rule instead of one literal rule per actor: the
/// `{principal}` segment captures the inbox owner and `bypass: ["{principal}"]`
/// lets that owner through — the compact shape docs/POLICY.md documents.
fn inbox_policy_principal_relative() -> String {
    r#"{
  "version": 1,
  "rules": [
    {
      "name": "inbox-owner-only",
      "match": { "refs": ["refs/collab/inbox/{principal}/**"] },
      "effect": {
        "protect": { "restricts": ["create", "update", "delete"], "bypass": ["{principal}"] }
      }
    }
  ]
}"#
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_inbox_pushes_are_gated_per_actor_in_token_mode() -> TestResult {
    let server = start_server_with_two_principals().await?;
    // put_repo is bearer-less; token mode needs the credential on create, so
    // the repo is created through the API with alice's token.
    let status = reqwest::Client::new()
        .put(format!("{}/t/secured", server.base_url))
        .bearer_auth("alice-s3cret")
        .send()
        .await?
        .status();
    assert!(status.is_success(), "repo create with bearer: {status}");

    let put = reqwest::Client::new()
        .put(format!(
            "{}/t/secured/policy",
            server.base_url
        ))
        .bearer_auth("alice-s3cret")
        .header("content-type", "application/json")
        .body(inbox_policy())
        .send()
        .await?;
    assert_eq!(put.status(), 204, "{}", put.text().await?);

    let src = TestRepo::synthetic(1, 1)?;

    // alice writes alice's inbox: allowed.
    let (ok, err) = push_inbox(&src, &server, "alice", "e1", "alice-s3cret");
    assert!(ok, "alice pushing her own inbox failed: {err}");

    // bob pushes alice's inbox: refused by the rule (transport identity wins
    // over any signature the payload may carry).
    let (ok, err) = push_inbox(&src, &server, "alice", "e2", "bob-s3cret");
    assert!(
        !ok,
        "bob pushing alice's inbox must be rejected; stderr: {err}"
    );
    assert!(
        err.contains("lock-alice-inbox") || err.contains("rejected by rule"),
        "stderr should name the rule: {err}"
    );

    // bob writes bob's own inbox: allowed.
    let (ok, err) = push_inbox(&src, &server, "bob", "e3", "bob-s3cret");
    assert!(ok, "bob pushing his own inbox failed: {err}");

    // Clearing the policy rolls the behavior back: bob can now push alice's
    // inbox (the read-side signature check remains the second layer).
    let cleared = reqwest::Client::new()
        .delete(format!(
            "{}/t/secured/policy",
            server.base_url
        ))
        .bearer_auth("bob-s3cret")
        .send()
        .await?;
    assert_eq!(cleared.status(), 204);
    let (ok, err) = push_inbox(&src, &server, "alice", "e4", "bob-s3cret");
    assert!(
        ok,
        "after policy clear the same push must succeed; stderr: {err}"
    );
    Ok(())
}

/// The compact principal-relative policy must behave exactly like the
/// per-actor literal rules above — one rule that scales with the team instead
/// of an admin edit per participant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_inbox_shards_with_one_principal_relative_rule() -> TestResult {
    let server = start_server_with_two_principals().await?;
    let status = reqwest::Client::new()
        .put(format!("{}/t/secured", server.base_url))
        .bearer_auth("alice-s3cret")
        .send()
        .await?
        .status();
    assert!(status.is_success(), "repo create with bearer: {status}");

    let put = reqwest::Client::new()
        .put(format!("{}/t/secured/policy", server.base_url))
        .bearer_auth("alice-s3cret")
        .header("content-type", "application/json")
        .body(inbox_policy_principal_relative())
        .send()
        .await?;
    assert_eq!(put.status(), 204, "{}", put.text().await?);

    let src = TestRepo::synthetic(1, 1)?;
    let (ok, err) = push_inbox(&src, &server, "alice", "p1", "alice-s3cret");
    assert!(ok, "alice pushing her own inbox failed: {err}");
    let (ok, err) = push_inbox(&src, &server, "bob", "p2", "bob-s3cret");
    assert!(ok, "bob pushing his own inbox failed: {err}");
    let (ok, err) = push_inbox(&src, &server, "alice", "p3", "bob-s3cret");
    assert!(
        !ok,
        "bob pushing alice's inbox must be rejected by the captured-owner rule; stderr: {err}"
    );
    assert!(
        err.contains("inbox-owner-only") || err.contains("rejected by rule"),
        "stderr should name the rule: {err}"
    );
    let (ok, err) = push_inbox(&src, &server, "bob", "p4", "alice-s3cret");
    assert!(
        !ok,
        "alice pushing bob's inbox must be rejected the same way; stderr: {err}"
    );
    Ok(())
}

/// The credential this suite pushes with must not survive into the text that feeds
/// `assert!(…, "{err}")` — those messages are printed into CI logs on failure. Lines are
/// shaped as measured against real `git 2.50.1` output in issue #145's review: the
/// `== Info:` URL line (still in the clear), a base64 `Authorization:` header (older gits;
/// 2.50 prints `<redacted>` itself), and the server's real rejection wording.
#[test]
fn failure_text_never_carries_the_credential() {
    let token = "alice-s3cret";
    let git_stderr = "\
== Info: Found bundle for host: 0x0 [serially]
== Info: Issue another request to this URL: 'http://git:alice-s3cret@127.0.0.1:4321/t/secured.git/info/refs?service=git-receive-pack'
=> Send header: POST /t/secured.git/info/refs?service=git-receive-pack HTTP/1.1
=> Send header: Authorization: Basic Z2l0OmFsaWNlLXMzY3JldA==
=> Send header: User-Agent: git/2.50.1
! [remote rejected] HEAD -> refs/collab/inbox/alice/e2 (rejected by rule 'lock-alice-inbox')
error: failed to push some refs to 'http://127.0.0.1:4321/t/secured.git'
";
    let scrubbed = scrub_credentials(git_stderr, token);
    assert!(
        !scrubbed.contains(token),
        "the literal token survived the scrub: {scrubbed}"
    );
    assert!(
        !scrubbed.to_ascii_lowercase().contains("authorization:"),
        "an Authorization header line survived the scrub: {scrubbed}"
    );
    assert!(
        !scrubbed.contains("Z2l0OmFsaWNlLXMzY3Jld"),
        "the base64 credential survived the scrub: {scrubbed}"
    );
    // The scrub must not cost the diagnosis: what the push was refused for, and why the
    // transport line exists at all, still read out afterwards.
    assert!(
        scrubbed.contains("rejected by rule 'lock-alice-inbox'")
            && scrubbed.contains("failed to push some refs")
            && scrubbed.contains("Issue another request to this URL"),
        "scrubbing removed more than credentials: {scrubbed}"
    );
}
