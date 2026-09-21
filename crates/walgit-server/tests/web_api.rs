#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::dbg_macro
)]
// Tests may panic freely (the intent recorded in clippy.toml); helpers outside
// #[test] fns are not covered by allow-*-in-tests, so each test target opts out.
//! web/API.md §6 conformance for the read-only JSON API.

mod harness;

use harness::{Server, git_in};
use serde_json::Value;

type TestResult = anyhow::Result<()>;

async fn get(
    server: &Server,
    path: &str,
) -> anyhow::Result<(reqwest::StatusCode, String, Option<String>)> {
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", server.base_url))
        .header("Accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);
    let text = resp.text().await?;
    Ok((status, text, ct))
}

async fn get_h(
    server: &Server,
    path: &str,
    extra: &[(&str, &str)],
) -> anyhow::Result<(reqwest::StatusCode, String, reqwest::header::HeaderMap)> {
    let mut req = reqwest::Client::new()
        .get(format!("{}{path}", server.base_url))
        .header("Accept", "application/json");
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = resp.text().await?;
    Ok((status, text, headers))
}
async fn head_h(
    server: &Server,
    path: &str,
) -> anyhow::Result<(reqwest::StatusCode, reqwest::header::HeaderMap)> {
    let resp = reqwest::Client::new()
        .head(format!("{}{path}", server.base_url))
        .send()
        .await?;
    Ok((resp.status(), resp.headers().clone()))
}

fn hdr(h: &reqwest::header::HeaderMap, k: &str) -> String {
    h.get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn json(server: &Server, path: &str) -> anyhow::Result<Value> {
    let (status, text, ct) = get(server, path).await?;
    anyhow::ensure!(status.is_success(), "GET {path} -> {status}: {text}");
    anyhow::ensure!(
        ct.as_deref().unwrap_or("").starts_with("application/json"),
        "content-type {ct:?}"
    );
    Ok(serde_json::from_str(&text)?)
}

/// Build a source repo with the shapes the UI cares about and push it.
fn fixture(server: &Server) -> anyhow::Result<std::path::PathBuf> {
    let dir = tempfile::tempdir()?.keep(); // TODO(hermetic): keep TempDir in fixture
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("README.md"), "# Title\n\nhello\n")?;
    std::fs::create_dir_all(dir.join("src/inner"))?;
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n")?;
    std::fs::write(dir.join("src/inner/x.txt"), "x\n")?;
    std::fs::write(dir.join("bin.dat"), [0u8, 159, 146, 150, 0, 1, 2])?;
    std::fs::write(dir.join("page.html"), "<script>alert(1)</script>")?;
    std::fs::write(dir.join("big.txt"), vec![b'a'; 2 * 1024 * 1024 + 1])?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "initial\n\nbody line"])?;
    // feature/x branch with a nested dir named after a path segment, plus rename.
    git_in(&dir, &["checkout", "-q", "-b", "feature/x"])?;
    std::fs::create_dir_all(dir.join("dir"))?;
    std::fs::write(dir.join("dir/f.txt"), "f\n")?;
    git_in(&dir, &["mv", "src/main.rs", "src/app.rs"])?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "feature work"])?;
    git_in(&dir, &["checkout", "-q", "main"])?;
    std::fs::write(dir.join("src/inner/x.txt"), "xx\n")?;
    git_in(&dir, &["commit", "-qam", "second on main"])?;
    git_in(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature/x"],
    )?;
    git_in(&dir, &["tag", "-a", "v1.0", "-m", "release"])?;
    git_in(
        &dir,
        &[
            "-c",
            "tag.forceSignAnnotated=false",
            "-c",
            "tag.gpgsign=false",
            "tag",
            "light",
        ],
    )?;
    for _ in 0..40 {
        git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "filler"])?;
    }
    // D1 collaboration namespace: arbitrary refs under refs/collab/* must be
    // hostable (the design's inbox model) and listable via the new endpoints.
    git_in(&dir, &["update-ref", "refs/collab/inbox/alice/1", "HEAD"])?;
    git_in(
        &dir,
        &["push", "-q", "--mirror", &server.repo_url("o", "r")],
    )?;
    Ok(dir)
}

/// `?raw` is the byte channel the blob viewers depend on: real content types,
/// ranges for media seeking, and a CSP that keeps repository HTML inert. It used
/// to answer only for text — every binary file came back as JSON `{binary:true}`,
/// so an image or a PDF could not be rendered at all.
// Same flavor as every other test that drives git: `fixture()` pushes with a
// *blocking* `git` child, and a current-thread runtime would starve the
// in-process server on its only thread — the push then hangs forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_serves_bytes_types_ranges_and_inert_html() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("o", "r").await?; // the fixture only pushes; the repo must exist
    fixture(&server)?;
    let c = reqwest::Client::new();
    let url = |p: &str| format!("{}{p}", server.base_url);

    // Bytes reach the client, with the type the viewer needs.
    let r = c.get(url("/o/r/api/blob/main/bin.dat?raw")).send().await?;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        r.bytes().await?.as_ref(),
        [0_u8, 159, 146, 150, 0, 1, 2],
        "the raw endpoint must not mangle binary"
    );

    // A single range is what `<video>`/`<audio>` seek with, and what a PDF uses
    // to load partially. "bytes 0-5/15" for a 15-byte README.
    let r = c
        .get(url("/o/r/api/blob/main/README.md?raw"))
        .header("range", "bytes=0-5")
        .send()
        .await?;
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 0-5/15");
    assert_eq!(r.headers()["accept-ranges"], "bytes");
    assert_eq!(r.text().await?, "# Titl");

    // Repository HTML must never run on the app's origin.
    let r = c.get(url("/o/r/api/blob/main/page.html?raw")).send().await?;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
    let csp = r.headers()["content-security-policy"].to_str()?;
    assert!(csp.contains("sandbox"), "active content needs a sandbox CSP: {csp}");
    // The word alone proves nothing: a policy that re-enabled scripts would
    // still contain it. Pin the two clauses that make it inert.
    assert!(!csp.contains("allow-scripts"), "scripts must stay disabled: {csp}");
    assert!(csp.contains("default-src 'none'"), "nothing may be loaded: {csp}");

    // The JSON lane is unchanged: the viewer still learns it is binary.
    let v: Value = c
        .get(url("/o/r/api/blob/main/bin.dat"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(v["binary"], true);
    assert!(v.get("contents").is_none());

    // The JSON lane's 2 MiB cap must not leak into the byte channel: `big.txt`
    // is 2 MiB + 1, which the JSON lane refuses and `?raw` must still serve.
    let r = c.get(url("/o/r/api/blob/main/big.txt?raw")).send().await?;
    assert_eq!(r.status(), 200, "the raw cap is its own budget");
    assert_eq!(r.bytes().await?.len(), 2 * 1024 * 1024 + 1);

    // A browser's Accept-Encoding must not turn the byte channel into a
    // compressed response (which would also drop Content-Length/Accept-Ranges).
    // `big.txt` (2 MiB + 1) is above the compression layer's size threshold, so
    // this fails if the byte channel is ever compressed — a 7-byte body would
    // pass regardless.
    let r = c
        .get(url("/o/r/api/blob/main/big.txt?raw"))
        .header("accept-encoding", "gzip, br")
        .send()
        .await?;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers().get("content-encoding").and_then(|v| v.to_str().ok()),
        Some("identity"),
        "raw bytes must not be re-encoded"
    );
    assert_eq!(r.bytes().await?.len(), 2 * 1024 * 1024 + 1);

    // A strong validator, and revalidation is a 304.
    let r = c.get(url("/o/r/api/blob/main/README.md?raw")).send().await?;
    let etag = r.headers()["etag"].to_str()?.to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    let r = c
        .get(url("/o/r/api/blob/main/README.md?raw"))
        .header("if-none-match", etag)
        .send()
        .await?;
    assert_eq!(r.status(), 304);
    Ok(())
}

/// web/API.md §6 against one server (called for the local-packs instance and
/// for a sibling that serves the same repo remotely).
async fn conformance(
    server: &Server,
    src: &std::path::Path,
    head: &str,
    feature: &str,
    v1_peeled: &str,
) -> TestResult {
    // owners
    assert_eq!(
        json(server, "/services/api/owners").await?,
        serde_json::json!(["o"])
    );
    assert_eq!(
        json(server, "/services/api/owners/o").await?,
        serde_json::json!(["r"])
    );

    // refs: O(1) head only, ETag + 304
    let (st, text, hdrs) = get_h(server, "/o/r/api/refs", &[]).await?;
    assert_eq!(st, 200);
    let refs: Value = serde_json::from_str(&text)?;
    assert_eq!(refs["head"]["name"], "main");
    assert_eq!(refs["head"]["sha"], head);
    let etag = hdr(&hdrs, "etag");
    assert_eq!(etag, format!("\"{head}\""));
    assert!(hdr(&hdrs, "cache-control").contains("stale-while-revalidate"));
    let (st, _, _) = get_h(server, "/o/r/api/refs", &[("If-None-Match", &etag)]).await?;
    assert_eq!(st, 304);
    // ref lists: paged, sorted, filtered
    let pg = json(server, "/o/r/api/refs/branches?n=1").await?;
    assert_eq!(pg["refs"][0]["name"], "feature/x");
    assert_eq!(pg["more"], true);
    let pg = json(server, "/o/r/api/refs/branches?after=feature/x&n=5").await?;
    assert_eq!(pg["refs"][0]["name"], "main");
    assert_eq!(pg["refs"][0]["sha"], head);
    assert_eq!(pg["more"], false);
    let pg = json(server, "/o/r/api/refs/branches?q=AIN").await?;
    assert_eq!(pg["refs"].as_array().unwrap().len(), 1);
    let pg = json(server, "/o/r/api/refs/branches?prefix=feature").await?;
    assert_eq!(pg["refs"][0]["name"], "feature/x");
    let pg = json(server, "/o/r/api/refs/tags").await?;
    let tags = pg["refs"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    let v1 = tags.iter().find(|tag| tag["name"] == "v1.0").unwrap();
    assert_eq!(v1["sha"], v1_peeled, "annotated tag sha must be peeled");
    assert_eq!(get(server, "/o/r/api/refs/nope").await?.0, 404);
    // SSE form
    let (st, body, hdrs) = get_h(
        server,
        "/o/r/api/refs/tags",
        &[("Accept", "text/event-stream")],
    )
    .await?;
    assert_eq!(st, 200);
    assert!(hdr(&hdrs, "content-type").starts_with("text/event-stream"));
    assert!(body.contains("event: ref\n") && body.contains("event: done\ndata: {\"more\":false}"));

    // any-namespace refs (D1 collab): full-name listing, namespace filter,
    // exact lookup, pagination, SSE
    let pg = json(server, "/o/r/api/refs/all").await?;
    let names: Vec<String> = pg["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    for expect in [
        "refs/heads/main",
        "refs/heads/feature/x",
        "refs/tags/v1.0",
        "refs/collab/inbox/alice/1",
    ] {
        assert!(
            names.contains(&expect.to_string()),
            "refs/all missing {expect}: {names:?}"
        );
    }
    assert!(
        names.windows(2).all(|pair| pair[0] < pair[1]),
        "refs/all must be byte-sorted: {names:?}"
    );
    let pg = json(server, "/o/r/api/refs/all?prefix=refs/collab").await?;
    assert_eq!(pg["refs"].as_array().unwrap().len(), 1);
    let pg = json(server, "/o/r/api/refs/all?n=2").await?;
    assert_eq!(pg["refs"].as_array().unwrap().len(), 2);
    assert_eq!(pg["more"], true);
    let pg = json(server, "/o/r/api/refs/collab").await?;
    assert_eq!(pg["refs"][0]["name"], "refs/collab/inbox/alice/1");
    assert_eq!(pg["refs"][0]["sha"], head);
    assert_eq!(
        json(server, "/o/r/api/refs/collab?q=INBOX").await?["refs"][0]["name"],
        "refs/collab/inbox/alice/1"
    );
    let row = json(server, "/o/r/api/refs/name/refs/collab/inbox/alice/1").await?;
    assert_eq!(row["name"], "refs/collab/inbox/alice/1");
    assert_eq!(row["sha"], head);
    let row = json(server, "/o/r/api/refs/name/refs/heads/main").await?;
    assert_eq!(row["sha"], head);
    assert_eq!(get(server, "/o/r/api/refs/name/refs/nope").await?.0, 404);
    let (st, body, hdrs) = get_h(
        server,
        "/o/r/api/refs/collab",
        &[("Accept", "text/event-stream")],
    )
    .await?;
    assert_eq!(st, 200);
    assert!(hdr(&hdrs, "content-type").starts_with("text/event-stream"));
    assert!(body.contains("event: ref\n") && body.contains("event: done\n"));

    // merge-base (D1 review primitive): local git and remote reader agree
    let expected_base = git_in(src, &["merge-base", "main", "feature/x"])?
        .trim()
        .to_string();
    let mb = json(server, "/o/r/api/merge-base?from=main&to=feature/x").await?;
    assert_eq!(
        mb["from"].as_str().unwrap().len(),
        40,
        "from resolved to a sha"
    );
    assert_eq!(mb["to"].as_str().unwrap().len(), 40, "to resolved to a sha");
    assert_eq!(mb["merge_base"], expected_base);
    let mb = json(server, "/o/r/api/merge-base?from=main&to=main").await?;
    assert_eq!(mb["merge_base"], mb["from"], "same revision -> itself");
    assert_eq!(
        get(server, "/o/r/api/merge-base?from=nope&to=main")
            .await?
            .0,
        404
    );

    // diff (D1 review primitive): name-status / stat / patch, local + remote
    let diff = json(
        server,
        "/o/r/api/diff?from=feature/x&to=main&format=name-status",
    )
    .await?;
    assert_eq!(diff["format"], "name-status");
    assert_eq!(diff["from"].as_str().unwrap().len(), 40);
    assert_eq!(diff["to"].as_str().unwrap().len(), 40);
    let ch = diff["changes"].as_array().unwrap();
    assert!(
        ch.iter()
            .any(|change| change["status"] == "M" && change["path"] == "src/inner/x.txt"),
        "second on main modified x.txt: {ch:?}"
    );
    let st = json(server, "/o/r/api/diff?from=feature/x&to=main&format=stat").await?;
    assert_eq!(st["format"], "stat");
    assert!(
        st["stats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|srow| srow["path"] == "src/inner/x.txt"),
        "stat lists x.txt"
    );
    let pg = json(server, "/o/r/api/diff?from=feature/x&to=main").await?;
    assert_eq!(pg["format"], "patch", "default format is patch");
    assert!(pg["patch"].as_str().unwrap().contains("diff --git"));
    let same = json(server, "/o/r/api/diff?from=main&to=main&format=name-status").await?;
    assert_eq!(same["changes"].as_array().unwrap().len(), 0);
    assert_eq!(
        get(server, "/o/r/api/diff?from=main&to=main&format=bogus")
            .await?
            .0,
        404
    );
    assert_eq!(get(server, "/o/r/api/diff?from=nope&to=main").await?.0, 404);

    // blame (D1 review primitive): porcelain parsed, local + remote agree
    let bl = json(server, "/o/r/api/blame/main/src/inner/x.txt").await?;
    assert_eq!(bl["path"], "src/inner/x.txt");
    assert_eq!(bl["sha"].as_str().unwrap().len(), 40);
    let lines = bl["blame"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["line"], 1);
    assert_eq!(lines[0]["text"], "xx", "main's x.txt content");
    assert!(
        !lines[0]["author"].as_str().unwrap().is_empty(),
        "author present"
    );
    assert!(
        lines[0]["summary"]
            .as_str()
            .unwrap()
            .contains("second on main"),
        "main's line attributed to second on main: {lines:?}"
    );
    let b2 = json(server, "/o/r/api/blame/feature/x/src/inner/x.txt").await?;
    let l2 = &b2["blame"][0];
    assert!(
        l2["summary"].as_str().unwrap().contains("initial"),
        "feature/x's x.txt came from initial: {l2:?}"
    );
    assert_eq!(get(server, "/o/r/api/blame/main/nope.txt").await?.0, 404);
    // the rename (src/main.rs -> src/app.rs on feature/x, merged to main):
    // followed on BOTH local and remote (issue #13) — the line keeps its
    // "initial" attribution through the rename, on this instance whichever
    // way it holds the packs.
    let br = json(server, "/o/r/api/blame/main/src/app.rs").await?;
    let rlines = br["blame"].as_array().unwrap();
    assert_eq!(rlines.len(), 1, "one line: {rlines:?}");
    assert!(
        rlines[0]["summary"].as_str().unwrap().contains("initial"),
        "rename followed to the original commit: {rlines:?}"
    );

    // archive (D1 review primitive): binary download, gzip/zip magic
    let resp = reqwest::Client::new()
        .get(format!("{}/o/r/api/archive/main", server.base_url))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|hv| hv.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("application/gzip"), "ct: {ct}");
    let bytes = resp.bytes().await?;
    assert!(bytes.len() > 100, "archive is non-trivial: {}", bytes.len());
    assert!(
        bytes.starts_with(b"\x1f\x8b"),
        "gzip magic: {:02x?}",
        &bytes[..2]
    );
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/o/r/api/archive/main?format=zip",
            server.base_url
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|hv| hv.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("application/zip"), "ct: {ct}");
    let bytes = resp.bytes().await?;
    assert!(bytes.starts_with(b"PK"), "zip magic: {:02x?}", &bytes[..2]);
    assert_eq!(
        get(server, "/o/r/api/archive/main?format=bogus").await?.0,
        404
    );
    assert_eq!(get(server, "/o/r/api/archive/nope").await?.0, 404);

    // resolve
    let (st, text, hdrs) = get_h(server, "/o/r/api/resolve/feature/x/dir", &[]).await?;
    assert_eq!(st, 200);
    let row: Value = serde_json::from_str(&text)?;
    assert_eq!(row["ref"], "feature/x");
    assert_eq!(row["sha"], feature);
    assert_eq!(row["path"], "dir");
    assert_eq!(row["kind"], "branch");
    assert_eq!(hdr(&hdrs, "etag"), format!("\"{feature}\""));
    let row = json(server, "/o/r/api/resolve/v1.0").await?;
    assert_eq!(row["kind"], "tag");
    assert_eq!(row["sha"], v1_peeled);
    let row = json(server, &format!("/o/r/api/resolve/{}/src", &head[..8])).await?;
    assert_eq!(row["kind"], "commit");
    assert_eq!(row["sha"], head);
    assert_eq!(row["path"], "src");
    let row = json(server, "/o/r/api/resolve/").await?;
    assert_eq!(row["ref"], "main");
    let (st, _, ct) = get(server, "/o/r/api/resolve/nope/x").await?;
    assert_eq!(st, 404);
    assert!(!ct.unwrap_or_default().contains("json"));

    // tree root
    let (st, text, hdrs) = get_h(server, "/o/r/api/tree/main", &[]).await?;
    assert_eq!(st, 200);
    let tree: Value = serde_json::from_str(&text)?;
    assert_eq!(tree["ref"], "main");
    assert_eq!(tree["sha"], head);
    assert_eq!(tree["path"], "");
    assert!(hdr(&hdrs, "cache-control").contains("stale-while-revalidate"));
    assert_eq!(hdr(&hdrs, "etag"), format!("\"{head}\""));
    let names: Vec<&str> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ent| ent["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["dir", "src", "README.md", "big.txt", "bin.dat", "page.html"],
        "dirs first, then byte order"
    );
    let src_entry = &tree["entries"][1];
    assert_eq!(src_entry["type"], "tree");
    assert_eq!(src_entry["mode"], "040000");
    assert_eq!(src_entry["size"], -1);
    let readme_entry = &tree["entries"][2];
    assert_eq!(readme_entry["type"], "blob");
    assert_eq!(readme_entry["mode"], "100644");
    assert_eq!(readme_entry["size"], "# Title\n\nhello\n".len());
    assert_eq!(tree["readme"]["name"], "README.md");
    assert!(
        tree["readme"]["contents"]
            .as_str()
            .unwrap()
            .starts_with("# Title")
    );
    assert_eq!(tree["commit"]["sha"].as_str().unwrap().len(), 40);

    // longest-ref rule: feature/x + path dir
    let treev = json(server, "/o/r/api/tree/feature/x/dir").await?;
    assert_eq!(treev["ref"], "feature/x");
    assert_eq!(treev["path"], "dir");
    assert_eq!(treev["entries"][0]["name"], "f.txt");
    // subtree commit = newest commit touching the path
    let treev = json(server, "/o/r/api/tree/main/src/inner").await?;
    assert_eq!(treev["entries"][0]["name"], "x.txt");
    assert_eq!(treev["commit"]["subject"], "second on main");
    // blob path as tree -> 404 plain text
    let (st, body, ct) = get(server, "/o/r/api/tree/main/README.md").await?;
    assert_eq!(st, 404);
    assert!(
        !ct.unwrap_or_default().contains("json"),
        "404 must be plain text: {body}"
    );
    // sha as ref -> immutable
    let (st, text, hdrs) = get_h(server, &format!("/o/r/api/tree/{feature}"), &[]).await?;
    assert_eq!(st, 200);
    let treev: Value = serde_json::from_str(&text)?;
    assert_eq!(treev["ref"], feature);
    assert_eq!(treev["sha"], feature);
    assert!(hdr(&hdrs, "cache-control").contains("immutable"));
    // second hit served from the immutable LRU
    let (st, text2, _) = get_h(server, &format!("/o/r/api/tree/{feature}"), &[]).await?;
    assert_eq!(st, 200);
    assert_eq!(text, text2);

    // blob
    let bl = json(server, "/o/r/api/blob/main/README.md").await?;
    assert_eq!(bl["name"], "README.md");
    assert_eq!(bl["path"], "README.md");
    assert_eq!(bl["contents"], "# Title\n\nhello\n");
    let (st, raw, ct) = get(server, "/o/r/api/blob/main/README.md?raw").await?;
    assert_eq!(st, 200);
    assert!(ct.unwrap_or_default().starts_with("text/plain"));
    assert_eq!(raw, "# Title\n\nhello\n");
    let bl = json(server, "/o/r/api/blob/main/bin.dat").await?;
    assert_eq!(bl["binary"], true);
    assert!(bl.get("contents").is_none());
    let bl = json(server, "/o/r/api/blob/main/big.txt").await?;
    assert_eq!(bl["too_large"], true);
    assert_eq!(bl["size"], 2 * 1024 * 1024 + 1);
    assert_eq!(get(server, "/o/r/api/blob/main/nope.txt").await?.0, 404);

    // commits + pagination
    let cl = json(server, "/o/r/api/commits?ref=main&path=&skip=0").await?;
    assert_eq!(cl["ref"], "main");
    assert_eq!(cl["sha"], head);
    let (_, _, hdrs) = get_h(
        server,
        &format!("/o/r/api/commits?ref={head}&path=&skip=0"),
        &[],
    )
    .await?;
    assert!(hdr(&hdrs, "cache-control").contains("immutable"));
    let commits = cl["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 35);
    assert_eq!(cl["more"], true);
    assert_eq!(commits[0]["sha"], head);
    assert!(commits[0]["parents"].is_array());
    let c2 = json(server, "/o/r/api/commits?ref=main&skip=35&n=50").await?;
    assert_eq!(c2["more"], false);
    let total = 35 + c2["commits"].as_array().unwrap().len();
    let expected: usize = git_in(src, &["rev-list", "--count", "main"])?
        .trim()
        .parse()?;
    assert_eq!(total, expected);
    let cl = json(server, "/o/r/api/commits?ref=main&path=src/inner/x.txt").await?;
    let subjects: Vec<&str> = cl["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|subj| subj["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, vec!["second on main", "initial"]);
    let first = &cl["commits"][1];
    assert_eq!(first["body"], "body line");
    assert_eq!(first["parents"], serde_json::json!([]));
    assert!(first["author_date"].as_str().unwrap().contains('T'));
    assert_eq!(get(server, "/o/r/api/commits?ref=nope").await?.0, 404);

    // commit detail: rename + merge (first-parent)
    let cmt = json(server, &format!("/o/r/api/commit/{feature}")).await?;
    assert_eq!(cmt["commit"]["sha"], feature);
    let paths: Vec<&str> = cmt["stats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|srow| srow["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"src/app.rs"),
        "renamed file appears once with new path: {paths:?}"
    );
    assert!(!paths.contains(&"src/main.rs"));
    assert!(cmt["patch"].as_str().unwrap().contains("diff --git a/"));
    let mg = json(server, &format!("/o/r/api/commit/{head}")).await?;
    // HEAD is a filler empty commit; find the merge commit instead.
    assert_eq!(mg["stats"], serde_json::json!([]));
    let merge = git_in(src, &["rev-parse", "main~40"])?.trim().to_string();
    let mg = json(server, &format!("/o/r/api/commit/{merge}")).await?;
    assert_eq!(mg["commit"]["parents"].as_array().unwrap().len(), 2);
    assert!(
        !mg["stats"].as_array().unwrap().is_empty(),
        "merge diffed against first parent must have stats"
    );
    assert!(mg["patch"].as_str().unwrap().contains("diff --git"));
    assert!(!mg["patch"].as_str().unwrap().contains("diff --cc"));
    // short sha and 404
    let cmt = json(server, &format!("/o/r/api/commit/{}", &feature[..10])).await?;
    assert_eq!(cmt["commit"]["sha"], feature);
    let (_, _, hdrs) = get_h(server, &format!("/o/r/api/commit/{}", &feature[..10]), &[]).await?;
    assert_eq!(hdr(&hdrs, "etag"), format!("\"{feature}\""));
    let (_, _, hdrs) = get_h(server, &format!("/o/r/api/commit/{feature}"), &[]).await?;
    assert!(hdr(&hdrs, "cache-control").contains("immutable"));
    let (st, _, ct) = get(server, "/o/r/api/commit/deadbeef").await?;
    assert_eq!(st, 404);
    assert!(!ct.unwrap_or_default().contains("json"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_md_conformance() -> TestResult {
    let server = Server::start().await?;
    // Empty instance.
    assert_eq!(
        json(&server, "/services/api/owners").await?,
        serde_json::json!([])
    );
    assert_eq!(
        json(&server, "/services/api/owners/nobody").await?,
        serde_json::json!([])
    );

    server.put_repo("o", "r").await?;
    let src = fixture(&server)?;
    let head = git_in(&src, &["rev-parse", "HEAD"])?.trim().to_string();
    let feature = git_in(&src, &["rev-parse", "feature/x"])?
        .trim()
        .to_string();
    let v1_peeled = git_in(&src, &["rev-list", "-1", "v1.0"])?
        .trim()
        .to_string();
    conformance(&server, &src, &head, &feature, &v1_peeled).await?;

    // unknown repo
    assert_eq!(get(&server, "/o/nope/api/refs").await?.0, 404);
    // page route -> index.html
    let (st, html, ct) = get(&server, "/o/r/tree/main/anything").await?;
    assert_eq!(st, 200);
    assert!(ct.unwrap_or_default().starts_with("text/html"));
    assert!(html.contains("<html"));
    // SPA collab pages serve index.html on a direct hit / refresh (#34).
    for path in [
        "/o/r/collab",
        "/o/r/collab/board",
        "/o/r/collab/guide",
        "/o/r/collab/thread/w1",
    ] {
        let (st, html, _) = get(&server, path).await?;
        assert_eq!(st, 200, "{path}");
        assert!(html.contains("<html"), "{path}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_repo_refs() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("o", "empty").await?;
    let refs = json(&server, "/o/empty/api/refs").await?;
    assert!(refs["head"].is_null());
    let p = json(&server, "/o/empty/api/refs/branches").await?;
    assert_eq!(p["refs"], serde_json::json!([]));
    assert_eq!(p["more"], false);
    assert_eq!(get(&server, "/o/empty/api/resolve/").await?.0, 404);
    Ok(())
}

/// The same contract on an instance whose `cache.max_bytes` is too small for
/// the repo's packs: objects are read from the store by range (indexes local),
/// nothing is materialized, and long answers stream the SSE envelope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_objects_conformance() -> TestResult {
    let big = Server::start().await?;
    big.put_repo("o", "r").await?;
    let src = fixture(&big)?;
    let head = git_in(&src, &["rev-parse", "HEAD"])?.trim().to_string();
    let feature = git_in(&src, &["rev-parse", "feature/x"])?
        .trim()
        .to_string();
    let v1_peeled = git_in(&src, &["rev-list", "-1", "v1.0"])?
        .trim()
        .to_string();

    let small = big
        .start_sibling_with(|cfg| {
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await?;

    // First object request with SSE accept: envelope with task/notice packets and a result.
    let resp = reqwest::Client::new()
        .get(format!("{}/o/r/api/tree/{head}", small.base_url))
        .header("Accept", "application/json, text/event-stream")
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(
        hdr(resp.headers(), "content-type").starts_with("text/event-stream"),
        "first remote render streams"
    );
    let text = resp.text().await?;
    assert!(
        text.contains("event: notice\n"),
        "narrates what it does: {text}"
    );
    assert!(
        text.contains("event: task\n"),
        "remote-index task announced: {text}"
    );

    let result_line = text
        .split("\n\n")
        .find(|p| p.starts_with("event: result"))
        .expect("result packet");
    let body: Value =
        serde_json::from_str(result_line.trim_start_matches("event: result\ndata: "))?;
    assert_eq!(body["sha"], head);
    assert!(body["entries"].as_array().unwrap().len() >= 5);

    // A blob above the JSON lane's 2 MiB cap must still come back through the
    // byte channel on a *remote-serving* host (the object is faulted from the
    // pack set: `cache.max_bytes = 1` guarantees the remote reader). This used to
    // answer 404 — the read was skipped on the JSON cap while `?raw` allowed 32
    // MiB, which broke exactly the 2–32 MiB images/PDFs/audio the channel exists
    // for.
    let resp = reqwest::Client::new()
        .get(format!("{}/o/r/api/blob/main/big.txt?raw", small.base_url))
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        200,
        "remote raw must be bounded by the byte channel's own cap"
    );
    assert_eq!(resp.bytes().await?.len(), 2 * 1024 * 1024 + 1);

    // Second time: served from the immutable cache as plain JSON even with SSE accept.
    let resp = reqwest::Client::new()
        .get(format!("{}/o/r/api/tree/{head}", small.base_url))
        .header("Accept", "application/json, text/event-stream")
        .send()
        .await?;
    assert!(hdr(resp.headers(), "content-type").starts_with("application/json"));
    assert!(hdr(resp.headers(), "cache-control").contains("immutable"));

    // Full contract, plain JSON.
    conformance(&small, &src, &head, &feature, &v1_peeled).await?;
    assert!(
        !small.registry_has_packs("o", "r").await,
        "small front must not materialize packs"
    );

    // Tasks are discoverable: the remote-index task ran here and finished ok.
    let t = json(&small, "/o/r/api/tasks").await?;
    let recent = t["recent"].as_array().unwrap();
    let ri = recent
        .iter()
        .find(|r| r["kind"] == "remote-index")
        .expect("remote-index task");
    assert_eq!(ri["ok"], true);
    assert!(t["running"].as_array().unwrap().is_empty());
    // Attach to the finished task: replay + result.
    let (st, text, h) = get_h(
        &small,
        &format!("/o/r/api/tasks/{}", ri["id"].as_str().unwrap()),
        &[("Accept", "text/event-stream")],
    )
    .await?;
    assert_eq!(st, 200);
    assert!(hdr(&h, "content-type").starts_with("text/event-stream"));
    assert!(text.contains("event: result\n"), "{text}");

    // Overview (WAL tab) renders without packs and says so.
    let o = json(&small, "/o/r/api/overview").await?;
    assert_eq!(o["local"]["objects"], "remote");
    assert!(
        o["health"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i.as_str().unwrap().contains("exceeds"))
    );
    Ok(())
}

/// D15/D27: the repo-scoped API is `/{owner}/{repo}/api/…` (and `…/api-browser/…`);
/// the pre-D15 `/services/api/{owner}/{repo}/…` shape is gone (no aliases —
/// AGENTS banner).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_api_prefix_is_gone() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("o", "r").await?;
    let src = fixture(&server)?;
    let _ = git_in(&src, &["rev-parse", "HEAD"])?;
    for path in ["refs", "resolve/main", "tree/main", "tasks", "overview"] {
        let (sa, a, _) = get_h(&server, &format!("/o/r/api/{path}"), &[]).await?;
        assert_eq!(sa, 200, "{path}: {a}");
        let (sb, _, _) = get_h(&server, &format!("/o/r/api-browser/{path}"), &[]).await?;
        assert_eq!(sb, 200, "{path} on the browser lane");
        let (sc, _, _) = get_h(&server, &format!("/services/api/o/r/{path}"), &[]).await?;
        assert_eq!(sc, 404, "{path}: /services/api/o/r must be gone");
    }
    Ok(())
}

/// Pushes are `/<area>/<repository>.git` only — no `.git` is a pkt-line ERR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_requires_area_repository_git() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("area", "repository").await?;
    let (st, body, _) = get_h(
        &server,
        "/area/repository/info/refs?service=git-receive-pack",
        &[],
    )
    .await?;
    assert_eq!(st, 200, "{body}");
    assert!(
        body.contains("<area>/<repository>.git"),
        "refusal must name the required URL shape: {body:?}"
    );
    let (ok, ad, _) = get_h(
        &server,
        "/area/repository.git/info/refs?service=git-receive-pack",
        &[],
    )
    .await?;
    assert_eq!(ok, 200, "{ad}");
    assert!(!ad.contains("push URL must be"), "{ad:?}");
    Ok(())
}

/// A browser on localhost is sent to walgit.localhost (same port). Git is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_localhost_redirects_to_walgit_localhost() -> TestResult {
    let server = Server::start().await?;
    let url = format!("{}/", server.base_url);
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(&url)
        .header("Accept", "text/html")
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::FOUND,
        "{}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("walgit.localhost"),
        "browser should be sent to walgit.localhost, got {loc}"
    );
    let git = reqwest::Client::new()
        .get(format!(
            "{}/area/repository.git/info/refs?service=git-upload-pack",
            server.base_url
        ))
        .header("User-Agent", "git/2.46.0")
        .send()
        .await?;
    assert_ne!(git.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_base_unrelated_histories() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("o", "r2").await?;
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("a.txt"), "a\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "a"])?;
    git_in(&dir, &["checkout", "-q", "--orphan", "other"])?;
    std::fs::write(dir.join("b.txt"), "b\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "b"])?;
    git_in(
        &dir,
        &["push", "-q", "--mirror", &server.repo_url("o", "r2")],
    )?;
    let mb = json(&server, "/o/r2/api/merge-base?from=main&to=other").await?;
    assert_eq!(mb["merge_base"], Value::Null, "unrelated histories -> null");
    Ok(())
}

/// `remote_blame_rename_boundary_over_tree_budget_is_503` shrinks
/// `WALGIT_TEST_BLAME_TREE_BUDGET` through the process-global env, and
/// `remote_blame_follows_exact_renames`'s concurrent request reads that same
/// knob; a rename-tree fault landing inside the knob's window would spuriously
/// 503. Serialize the two on this lock: the knob test holds it from
/// `set_var` to `remove_var`, the rename test for its whole run.
///
/// The guard is deliberately held across awaits — this is a test-serialization
/// barrier, not a resource lock, so `clippy::await_holding_lock` is allowed on
/// the two tests below.
static BLAME_ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Remote blame follows renames (issue #13): git blame's rename detection
/// reads the old path's tree in the parent, so the walk faults the boundary
/// parent's whole tree skeleton and continues along the exact content
/// predecessor. Local and remote must then agree byte-for-byte.
#[allow(clippy::await_holding_lock)] // serialization barrier, see BLAME_ENV_LOCK
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_blame_follows_exact_renames() -> TestResult {
    // Held across every await so the sibling knob test cannot interleave.
    let _env_lock = BLAME_ENV_LOCK.lock();
    let big = Server::start().await?;
    big.put_repo("o", "r").await?;
    fixture(&big)?;
    let small = big
        .start_sibling_with(|cfg| {
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await?;
    // Local blame follows the rename (src/main.rs -> src/app.rs) and works.
    let b = json(&big, "/o/r/api/blame/main/src/app.rs").await?;
    assert_eq!(b["path"], "src/app.rs");
    assert!(
        !b["blame"].as_array().unwrap().is_empty(),
        "local blame works"
    );
    // Remote follows the same rename and answers identically.
    let rb = json(&small, "/o/r/api/blame/main/src/app.rs").await?;
    assert_eq!(rb, b, "remote blame matches local through the rename");
    Ok(())
}

/// A rename boundary whose parent tree exceeds the skeleton budget is a
/// defined 503, not a 404 or a wrong answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The knob is environment-read at request time (the WALGIT_TEST_* pattern),
// and `std::env::set_var` is unsafe in this edition; contained to this test.
#[allow(unsafe_code)]
#[allow(clippy::await_holding_lock)] // serialization barrier, see BLAME_ENV_LOCK
async fn remote_blame_rename_boundary_over_tree_budget_is_503() -> TestResult {
    // Held from before `set_var` until after `remove_var` so the sibling
    // rename-following test never reads the shrunk knob mid-request.
    let _env_lock = BLAME_ENV_LOCK.lock();
    let big = Server::start().await?;
    big.put_repo("o", "r").await?;
    fixture(&big)?;
    let small = big
        .start_sibling_with(|cfg| {
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await?;
    // SAFETY: the knob's only readers in this process are the two blame tests,
    // serialized on BLAME_ENV_LOCK; other env access is the WALGIT_TEST_* norm.
    unsafe { std::env::set_var("WALGIT_TEST_BLAME_TREE_BUDGET", "1") };
    let (st, body, _) = get(&small, "/o/r/api/blame/main/src/app.rs").await?;
    // SAFETY: restoring under the same lock, before the guard drops.
    unsafe { std::env::remove_var("WALGIT_TEST_BLAME_TREE_BUDGET") };
    assert_eq!(
        st, 503,
        "boundary tree over budget must be 503, got: {body}"
    );
    assert!(
        body.contains("rename boundary"),
        "503 names the boundary: {body}"
    );
    Ok(())
}

/// Regression (PR #9 review C1): the remote merge-base walk must not busy-spin
/// when one frontier exhausts before the other. Two unrelated roots on a
/// 1-byte-cache sibling must answer `null`, not hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_merge_base_unrelated_histories_is_null() -> TestResult {
    let big = Server::start().await?;
    big.put_repo("o", "r").await?;
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("a.txt"), "a\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "a"])?;
    git_in(&dir, &["checkout", "-q", "--orphan", "other"])?;
    std::fs::write(dir.join("b.txt"), "b\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "b"])?;
    git_in(&dir, &["push", "-q", "--mirror", &big.repo_url("o", "r")])?;
    let small = big
        .start_sibling_with(|cfg| {
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await?;
    let mb = json(&small, "/o/r/api/merge-base?from=main&to=other").await?;
    assert_eq!(mb["merge_base"], Value::Null, "remote unrelated -> null");
    Ok(())
}

/// Regression (PR #9 review C1): a feature forked from DEEP main — the walk
/// must meet at the branch point even though one side's frontier empties
/// first, and the remote answer must equal local git's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_merge_base_deep_fork() -> TestResult {
    let big = Server::start().await?;
    big.put_repo("o", "r").await?;
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("a.txt"), "a\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "c1"])?;
    for i in 2..=10 {
        git_in(
            &dir,
            &["commit", "-q", "--allow-empty", "-m", &format!("m{i}")],
        )?;
    }
    // Feature forks from main~2 (deep in main's history) and adds two commits.
    git_in(&dir, &["checkout", "-q", "-b", "feature", "HEAD~2"])?;
    git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "f1"])?;
    git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "f2"])?;
    git_in(&dir, &["checkout", "-q", "main"])?;
    let expected = git_in(&dir, &["merge-base", "main", "feature"])?
        .trim()
        .to_string();
    git_in(&dir, &["push", "-q", "--mirror", &big.repo_url("o", "r")])?;
    let small = big
        .start_sibling_with(|cfg| {
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await?;
    let mb = json(&small, "/o/r/api/merge-base?from=main&to=feature").await?;
    assert_eq!(
        mb["merge_base"], expected,
        "remote deep fork base matches git"
    );
    Ok(())
}

/// D1 thin-API write path: POST a signed collab entry -> the ref lands in
/// refs/collab/inbox/<actor>/; posting as someone else is forbidden; no
/// credential is 401. (Signature verification is client-side; the server
/// enforces identity and inbox ownership.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_thin_api_posts_signed_entries() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "alice".into(),
            token: "alice-token".into(),
            token_env: None,
            write: true,
            admin: false,
        }];
    })
    .await?;
    let client = reqwest::Client::new();
    let put = client
        .put(format!("{}/o/r", server.base_url))
        .bearer_auth("alice-token")
        .send()
        .await?;
    assert!(put.status().is_success() || put.status() == reqwest::StatusCode::CONFLICT);
    let url = format!("{}/o/r/api/collab/entries", server.base_url);

    let entry = serde_json::json!({
        "version": 1, "kind": "issue", "id": "t1", "actor": "alice",
        "ts": 1_786_500_000, "parent": "", "body": {"title": "hi"},
        "sig": "ed25519:AAAA"
    });
    let resp = client
        .post(&url)
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "entry": entry }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "post signed entry");
    let body: serde_json::Value = resp.json().await?;
    let ref_name = body["ref"].as_str().expect("ref").to_string();
    assert!(
        ref_name.starts_with("refs/collab/inbox/alice/"),
        "{ref_name}"
    );
    assert_eq!(body["oid"].as_str().unwrap().len(), 40);

    // Visible in the collab namespace listing (authenticated read).
    let (st, text, _) = get_h(
        &server,
        "/o/r/api/refs/collab",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    let r: serde_json::Value = serde_json::from_str(&text)?;
    let names: Vec<String> = r["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&ref_name), "ref in refs/collab: {names:?}");

    // Posting as someone else -> 403.
    let bad = serde_json::json!({
        "entry": serde_json::json!({
            "version": 1, "kind": "comment", "id": "t1", "actor": "bob",
            "ts": 1, "parent": "", "body": {}, "sig": ""
        })
    });
    let resp = client
        .post(&url)
        .bearer_auth("alice-token")
        .json(&bad)
        .send()
        .await?;
    let bad_status = resp.status();
    assert_eq!(bad_status, 403, "actor != principal refused");

    // No credential -> 401 challenge (§1.3 tells the why); the web lane is
    // Bearer-only — `Basic` must never reach a browser-reachable surface (#91).
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "entry": entry }))
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        401,
        "unauthenticated refused (token mode, no credential)"
    );
    let www = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        www.starts_with("bearer") && !www.contains("basic"),
        "web lane is Bearer-only, got WWW-Authenticate: {www}"
    );

    // Same contract on the v1 lane the SDK popup lands on (`/api/v1/me` goes
    // through the same mapper): 401, Bearer-only.
    let resp = client
        .get(format!("{}/api/v1/me", server.base_url))
        .send()
        .await?;
    assert_eq!(resp.status(), 401, "{}", resp.status());
    let www = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        www.starts_with("bearer") && !www.contains("basic"),
        "v1 lane is Bearer-only, got WWW-Authenticate: {www}"
    );
    Ok(())
}

/// §1.3 / #93: an anonymous (credential-less) call to an admin surface
/// (settings, policy) is a challenge — 401, Bearer-only (web lane) — not a
/// 403 the client cannot act on; an authenticated non-admin identity keeps
/// its real 403 (a retry cannot help).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_surfaces_challenge_anonymous_and_forbid_non_admin() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "adm@example.com".into(),
                token: "adm".into(),
                token_env: None,
                write: true,
                admin: true,
            },
            walgit_config::StaticToken {
                principal: "dev@example.com".into(),
                token: "dev".into(),
                token_env: None,
                write: true,
                admin: false,
            },
        ];
    })
    .await?;
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/t/adm", server.base_url))
        .bearer_auth("adm")
        .send()
        .await?;
    assert!(
        resp.status().is_success() || resp.status() == reqwest::StatusCode::CONFLICT,
        "create repo: {}",
        resp.status()
    );
    let policy_body = r#"{"version":1,"groups":[],"rules":[]}"#;

    // Anonymous (no credential) on both admin surfaces: 401, Bearer-only.
    for (url, body) in [
        (
            format!("{}/t/adm/api/settings?message=x", server.base_url),
            "[bundles]\n".to_string(),
        ),
        (
            format!("{}/t/adm/api/policy", server.base_url),
            policy_body.to_string(),
        ),
    ] {
        let resp = client.put(&url).body(body).send().await?;
        assert_eq!(resp.status(), 401, "{url}: {}", resp.status());
        let www = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            www.starts_with("bearer") && !www.contains("basic"),
            "{url}: admin surface challenges Bearer-only, got {www}"
        );
    }

    // Authenticated non-admin: a real 403 on both.
    for url in [
        format!("{}/t/adm/api/settings?message=x", server.base_url),
        format!("{}/t/adm/api/policy", server.base_url),
    ] {
        let resp = client
            .put(&url)
            .bearer_auth("dev")
            .body("[bundles]\n")
            .send()
            .await?;
        assert_eq!(resp.status(), 403, "{url}: {}", resp.status());
    }

    // Admin: through.
    let resp = client
        .put(format!("{}/t/adm/api/settings?message=x", server.base_url))
        .bearer_auth("adm")
        .body("[bundles]\n")
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "{}", resp.text().await?);
    let resp = client
        .put(format!("{}/t/adm/api/policy", server.base_url))
        .bearer_auth("adm")
        .body(policy_body)
        .send()
        .await?;
    assert!(
        resp.status() == 200 || resp.status() == 204,
        "{}",
        resp.status()
    );
    Ok(())
}

/// D1 aggregation read path: after posting entries, `/api/collab/report` and
/// `/api/collab/threads/{id}` answer with the deterministic aggregation
/// (thread summaries, ordered entries, PR view + merge rule evaluation).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_report_and_thread_aggregate_entries() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "alice".into(),
            token: "alice-token".into(),
            token_env: None,
            write: true,
            admin: false,
        }];
    })
    .await?;
    let client = reqwest::Client::new();
    let put = client
        .put(format!("{}/o/r", server.base_url))
        .bearer_auth("alice-token")
        .send()
        .await?;
    assert!(put.status().is_success() || put.status() == reqwest::StatusCode::CONFLICT);
    let url = format!("{}/o/r/api/collab/entries", server.base_url);

    let issue = serde_json::json!({
        "version": 1, "kind": "issue", "id": "t1", "actor": "alice",
        "ts": 1_786_500_000, "parent": "", "body": {"title": "hi"},
        "sig": "ed25519:AAAA"
    });
    let patch = serde_json::json!({
        "version": 1, "kind": "patch", "id": "t1", "actor": "alice",
        "ts": 1_786_500_001, "parent": "", "body": {},
        "refs": {"base": "refs/heads/main", "head": "refs/heads/topic"},
        "sig": "ed25519:BBBB"
    });
    let review = serde_json::json!({
        "version": 1, "kind": "review", "id": "t1", "actor": "alice",
        "ts": 1_786_500_002, "parent": "", "body": {"decision": "approve"},
        "sig": "ed25519:CCCC"
    });
    let mut oids = Vec::new();
    for e in [&issue, &patch, &review] {
        let resp = client
            .post(&url)
            .bearer_auth("alice-token")
            .json(&serde_json::json!({ "entry": e }))
            .send()
            .await?;
        assert_eq!(resp.status(), 200, "post entry");
        let body: serde_json::Value = resp.json().await?;
        oids.push(body["oid"].as_str().unwrap().to_string());
    }

    let (st, text, _) = get_h(
        &server,
        "/o/r/api/collab/report",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    let report: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(report["total_entries"], 3);
    assert_eq!(report["threads"].as_array().unwrap().len(), 1);
    assert_eq!(report["threads"][0]["id"], "t1");
    assert_eq!(report["threads"][0]["title"], "hi", "root title projected (issue #131)");
    assert_eq!(report["threads"][0]["entries"], 3);
    assert_eq!(report["prs"].as_array().unwrap().len(), 1);
    assert_eq!(report["prs"][0]["title"], "hi");
    assert_eq!(report["prs"][0]["base"], "refs/heads/main");
    assert_eq!(report["prs"][0]["head"], "refs/heads/topic");
    assert_eq!(report["prs"][0]["status"], "open");
    // Default rules protect nothing -> merge allowed.
    assert_eq!(report["prs"][0]["merge_allowed"], true);

    let (st, text, _) = get_h(
        &server,
        "/o/r/api/collab/threads/t1",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    let thread: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(thread["id"], "t1");
    let entries = thread["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| e["entry"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["issue", "patch", "review"], "parent-ordered");
    let pr = thread["pr"]
        .as_object()
        .expect("thread has a patch -> pr view");
    assert_eq!(pr["pr"]["base"], "refs/heads/main");
    assert_eq!(pr["pr"]["head"], "refs/heads/topic");
    assert_eq!(pr["pr"]["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(pr["merge"]["allowed"], true);

    // Unknown thread -> 404.
    let (st, _, _) = get_h(
        &server,
        "/o/r/api/collab/threads/nope",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 404);
    Ok(())
}

/// D1 principal registration thin API + signed-entry verification: register
/// the authenticated principal's Ed25519 key, post a signed issue, and the
/// report/thread answers count it verified (the aggregation verifies exactly
/// what the CLI does locally).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_principal_registration_and_verified_entries() -> TestResult {
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use walgit_wal::collab::{Entry, sign_entry};

    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "alice".into(),
            token: "alice-token".into(),
            token_env: None,
            write: true,
            admin: false,
        }];
    })
    .await?;
    let client = reqwest::Client::new();
    let put = client
        .put(format!("{}/o/r", server.base_url))
        .bearer_auth("alice-token")
        .send()
        .await?;
    assert!(put.status().is_success() || put.status() == reqwest::StatusCode::CONFLICT);

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());

    // Register alice's key through the thin API.
    let resp = client
        .post(format!("{}/o/r/api/collab/principal", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "principal": "alice", "public_key": public_key }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "register principal");
    let body: serde_json::Value = resp.json().await?;
    let ref_name = body["ref"].as_str().unwrap();
    assert_eq!(ref_name, "refs/collab/meta/principals/alice");

    // Posting a registration for someone else -> 403.
    let resp = client
        .post(format!("{}/o/r/api/collab/principal", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "principal": "bob", "public_key": public_key }))
        .send()
        .await?;
    assert_eq!(resp.status(), 403, "registering another principal refused");

    // Post a genuinely signed issue entry.
    let mut entry = Entry {
        version: 1,
        kind: "issue".into(),
        id: "t2".into(),
        actor: "alice".into(),
        ts: 1_786_500_010,
        parent: String::new(),
        refs: None,
        body: serde_json::json!({ "title": "signed" }),
        sig: String::new(),
    };
    entry.sig = sign_entry(&mut entry, &sk);
    let resp = client
        .post(format!("{}/o/r/api/collab/entries", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "entry": serde_json::to_value(&entry)? }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "post signed entry");

    // The report counts it verified; the thread detail marks the entry verified.
    let (st, text, _) = get_h(
        &server,
        "/o/r/api/collab/report",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    let report: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(report["total_entries"], 1);
    assert_eq!(
        report["verified_entries"], 1,
        "signed entry with registered key verifies"
    );
    assert_eq!(report["unverified_entries"], 0);
    assert_eq!(report["missing_principals"], 0);
    assert_eq!(report["threads"][0]["verified"], 1);

    let (st, text, _) = get_h(
        &server,
        "/o/r/api/collab/threads/t2",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    let thread: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(thread["entries"][0]["verified"], true);
    Ok(())
}

/// Cross-language golden vector (cc-ai-d1-protocol-followups P0): the exact
/// bytes the SDK signs (`web/src/collab-canonical.test.ts`; Rust twin in
/// `walgit-wal`'s `golden_tests`) verify end-to-end through the thin API and
/// the aggregation. Before the SDK canonical fix, browser-signed entries
/// landed here unverified forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_sdk_golden_entry_verifies_end_to_end() -> TestResult {
    const GOLDEN_CANONICAL: &str = r#"{"actor":"alice","body":{"title":"golden vector"},"id":"golden","kind":"issue","parent":"","sig":"","ts":1786500000,"version":1}"#;
    const GOLDEN_SIG_B64: &str = "VFROsCUBDR4Sj1eFoMdDI/iRfV0A0jgRSGFGjAB91MVh2oh3IwnohAxj7Mq55x+uvpyrhM2tlq6x3WYuT9f5DQ==";
    const GOLDEN_PUB_B64: &str = "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";

    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "alice".into(),
            token: "alice-token".into(),
            token_env: None,
            write: true,
            admin: false,
        }];
    })
    .await?;
    let client = reqwest::Client::new();
    let put = client
        .put(format!("{}/o/r", server.base_url))
        .bearer_auth("alice-token")
        .send()
        .await?;
    assert!(put.status().is_success() || put.status() == reqwest::StatusCode::CONFLICT);

    let resp = client
        .post(format!("{}/o/r/api/collab/principal", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "principal": "alice", "public_key": GOLDEN_PUB_B64 }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "register golden principal");

    // The stored entry is the canonical document with `sig` filled in —
    // exactly what the SDK's `collab.buildEntry` returns.
    let mut entry: serde_json::Value = serde_json::from_str(GOLDEN_CANONICAL)?;
    entry["sig"] = serde_json::json!(format!("ed25519:{GOLDEN_SIG_B64}"));
    let resp = client
        .post(format!("{}/o/r/api/collab/entries", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "entry": entry }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "post golden entry");

    let (st, text, _) = get_h(
        &server,
        "/o/r/api/collab/report",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    let report: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(
        report["verified_entries"], 1,
        "the SDK's golden bytes must verify: {report}"
    );
    assert_eq!(report["unverified_entries"], 0);
    Ok(())
}

/// The thin API must honor `policy.json` exactly like receive-pack: a frozen
/// collab namespace blocks the browser path too (one ref, one guard level —
/// review finding MJ3 on PR #27). The refusal reason is logged; the answer is
/// the lane's `403`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_thin_api_honors_repo_policy() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "alice".into(),
                token: "alice-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
            walgit_config::StaticToken {
                principal: "root".into(),
                token: "root-token".into(),
                token_env: None,
                write: true,
                admin: true,
            },
        ];
    })
    .await?;
    let client = reqwest::Client::new();
    let put = client
        .put(format!("{}/o/r", server.base_url))
        .bearer_auth("alice-token")
        .send()
        .await?;
    assert!(put.status().is_success() || put.status() == reqwest::StatusCode::CONFLICT);

    // Admin freezes the whole collab namespace (no bypass list).
    let policy = r#"{
      "version": 1,
      "rules": [
        { "name": "freeze-collab",
          "match": { "refs": ["refs/collab/**"] },
          "effect": { "protect": { "restricts": ["create", "update", "delete"] } } }
      ]
    }"#;
    let resp = client
        .put(format!("{}/o/r/policy", server.base_url))
        .header("content-type", "application/json")
        .bearer_auth("root-token")
        .body(policy)
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        204,
        "{}",
        resp.text().await.unwrap_or_default()
    );

    let entry = serde_json::json!({
        "version": 1, "kind": "issue", "id": "t9", "actor": "alice",
        "ts": 1, "parent": "", "body": {}, "sig": ""
    });
    let resp = client
        .post(format!("{}/o/r/api/collab/entries", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "entry": entry }))
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        403,
        "policy denies the collab create on the thin path"
    );

    // Lift the freeze: the same write now lands.
    let del = client
        .delete(format!("{}/o/r/policy", server.base_url))
        .bearer_auth("root-token")
        .send()
        .await?;
    assert_eq!(del.status(), 204);
    let resp = client
        .post(format!("{}/o/r/api/collab/entries", server.base_url))
        .bearer_auth("alice-token")
        .json(&serde_json::json!({ "entry": entry }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "unfrozen collab writes again");
    Ok(())
}

/// D1 board (`GET /{o}/{r}/api/collab/board`): the deterministic projection
/// under the DEFAULT board (the repository defines none), auth like every
/// repo-scoped read; a thread lands in its effective-status lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_board_projects_threads_under_the_default_definition() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "alice".into(),
            token: "alice-token".into(),
            token_env: None,
            write: true,
            admin: false,
        }];
    })
    .await?;
    let client = reqwest::Client::new();
    let put = client
        .put(format!("{}/o/r", server.base_url))
        .bearer_auth("alice-token")
        .send()
        .await?;
    assert!(put.status().is_success() || put.status() == reqwest::StatusCode::CONFLICT);
    let url = format!("{}/o/r/api/collab/entries", server.base_url);
    for e in [
        serde_json::json!({
            "version": 1, "kind": "issue", "id": "t1", "actor": "alice",
            "ts": 1_786_500_000, "parent": "", "body": {"title": "hi"}, "sig": "ed25519:AAAA"
        }),
        serde_json::json!({
            "version": 1, "kind": "status", "id": "t1", "actor": "alice",
            "ts": 1_786_500_001, "parent": "", "body": {"status": "needs-review"}, "sig": "ed25519:BBBB"
        }),
    ] {
        let resp = client
            .post(&url)
            .bearer_auth("alice-token")
            .json(&serde_json::json!({ "entry": e }))
            .send()
            .await?;
        assert_eq!(resp.status(), 200, "post entry");
    }

    // No credential -> a real 401 (token mode).
    let (st, _, _) = get(&server, "/o/r/api/collab/board").await?;
    assert_eq!(st, 401);

    let (st, text, headers) = get_h(
        &server,
        "/o/r/api/collab/board",
        &[("Authorization", "Bearer alice-token")],
    )
    .await?;
    assert_eq!(st, 200);
    // Ref-dependent answer: SWR, never immutable.
    assert!(hdr(&headers, "cache-control").contains("stale-while-revalidate"));
    let board: serde_json::Value = serde_json::from_str(&text)?;
    let columns = board["columns"].as_array().unwrap();
    assert_eq!(
        columns.iter().map(|c| c["name"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["open", "merged", "closed", "other"],
        "the built-in default board"
    );
    let open = &columns[0]["cards"];
    assert_eq!(open.as_array().unwrap().len(), 0);
    let other = &columns[3]["cards"];
    assert_eq!(other[0]["id"], "t1");
    assert_eq!(other[0]["status"], "needs-review", "the status entry moved the card");
    assert_eq!(other[0]["title"], "hi");
    Ok(())
}

/// `docs/D1_CI_PROTOCOL.md` §8.2 storage convention (issue #161): the HTTP read side of
/// `refs/collab/ci-artifacts/<actor>/<sha256>` — exact bytes, immutable
/// caching, sha256-verified, 400/404 shapes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_ci_artifact_serves_verified_bytes() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("o", "r").await?;
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("f.txt"), "x\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "init"])?;

    let payload = b"artifact-payload-42";
    let sha256 = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(payload));
    std::fs::write(dir.join("app.bin"), payload)?;
    let oid = git_in(&dir, &["hash-object", "-w", "app.bin"])?;
    let oid = oid.trim();
    git_in(
        &dir,
        &[
            "update-ref",
            &format!("refs/collab/ci-artifacts/ci-a/{sha256}"),
            oid,
        ],
    )?;
    // A hostile same-address ref sorted before the valid publisher must not
    // shadow the real bytes.
    std::fs::write(dir.join("evil.bin"), b"not-the-payload")?;
    let evil_oid = git_in(&dir, &["hash-object", "-w", "evil.bin"])?;
    git_in(
        &dir,
        &[
            "update-ref",
            &format!("refs/collab/ci-artifacts/aaa/{sha256}"),
            evil_oid.trim(),
        ],
    )?;
    // A hostile ref: its name claims a sha256 the payload does not hash to.
    let bogus = "00".repeat(32);
    git_in(
        &dir,
        &[
            "update-ref",
            &format!("refs/collab/ci-artifacts/ci-a/{bogus}"),
            oid,
        ],
    )?;
    git_in(
        &dir,
        &["push", "-q", "--mirror", &server.repo_url("o", "r")],
    )?;

    let (st, body, headers) = get_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{sha256}"),
        &[],
    )
    .await?;
    assert_eq!(st, 200, "{body}");
    assert_eq!(hdr(&headers, "content-type"), "application/octet-stream");
    assert!(
        hdr(&headers, "cache-control").contains("immutable"),
        "immutable bytes: {}",
        hdr(&headers, "cache-control")
    );
    assert_eq!(hdr(&headers, "etag"), format!("\"{sha256}\""));
    assert_eq!(body.as_bytes(), payload);

    let missing = "11".repeat(32);
    let (st, _, _) = get_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{missing}"),
        &[],
    )
    .await?;
    assert_eq!(st, 404);
    let (st, _, _) = get_h(&server, "/o/r/api/collab/ci-artifacts/not-hex", &[]).await?;
    assert_eq!(st, 400, "a malformed address is a bad request");
    let (st, _, _) = get_h(
        &server,
        &format!(
            "/o/r/api/collab/ci-artifacts/{}",
            sha256.to_uppercase()
        ),
        &[],
    )
    .await?;
    assert_eq!(st, 400, "only lowercase sha256 addresses are canonical");
    let (st, _, _) = get_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{bogus}"),
        &[],
    )
    .await?;
    assert_eq!(st, 404, "content mismatch is not the artifact");

    let oversize = vec![0u8; usize::try_from(walgit_wal::ci::CI_ARTIFACT_MAX_BYTES).unwrap() + 1];
    std::fs::write(dir.join("oversize.bin"), &oversize)?;
    let oversize_oid = git_in(&dir, &["hash-object", "-w", "oversize.bin"])?;
    let oversize_sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&oversize));
    let oversize_ref = format!("refs/collab/ci-artifacts/ci-a/{oversize_sha}");
    git_in(
        &dir,
        &["update-ref", &oversize_ref, oversize_oid.trim()],
    )?;
    git_in(
        &dir,
        &["push", "-q", &server.repo_url("o", "r"), &oversize_ref],
    )?;
    let (st, _, _) = get_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{oversize_sha}"),
        &[],
    )
    .await?;
    assert_eq!(st, 413, "oversize objects are refused before body materialization");

    let (st, _) = head_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{sha256}?actor=ci-a"),
    )
    .await?;
    assert_eq!(st, 200, "a fitting exact actor is preflighted");
    let (st, _) = head_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{sha256}?actor=missing"),
    )
    .await?;
    assert_eq!(st, 404, "the preflight is scoped to the exact actor");
    let (st, _) = head_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{oversize_sha}?actor=ci-a"),
    )
    .await?;
    assert_eq!(st, 413, "the CLI preflight refuses before fetching the body");
    let (st, _) = head_h(
        &server,
        &format!("/o/r/api/collab/ci-artifacts/{sha256}?actor=bad%2Factor"),
    )
    .await?;
    assert_eq!(st, 400, "actor is a single ref segment");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_snapshot_over_cap_is_rejected_before_body_read() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("o", "big").await?;
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "init"])?;

    let size = 64usize * 1024 * 1024 + 1;
    let oversize = vec![0u8; size];
    std::fs::write(dir.join("snapshot.bin"), &oversize)?;
    let oid = git_in(&dir, &["hash-object", "-w", "snapshot.bin"])?;
    let snapshot_ref = walgit_wal::collab::SNAPSHOT_REF;
    git_in(&dir, &["update-ref", snapshot_ref, oid.trim()])?;
    git_in(
        &dir,
        &["push", "-q", &server.repo_url("o", "big"), snapshot_ref],
    )?;

    let (st, _, _) = get_h(&server, "/o/big/api/collab/report", &[]).await?;
    assert_eq!(st, 503, "oversize snapshot is rejected before body materialization");
    Ok(())
}
