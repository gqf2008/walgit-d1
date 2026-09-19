#!/usr/bin/env python3
"""End-to-end smoke for `walgit mcp` resource subscriptions.

The test uses a local bare repository as `origin`, so it exercises the same
client-side pull lane an MCP host uses without needing a server or bucket. It
covers refs, cross-checkout collab board/thread updates, disappearance
notifications, unsubscribe, and active-subscription shutdown.
"""

from __future__ import annotations

import json
import os
import queue
import secrets
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


class Mcp:
    def __init__(
        self,
        binary: Path,
        config: Path,
        repo: Path,
        env: dict[str, str] | None = None,
    ) -> None:
        self.proc = subprocess.Popen(
            [
                str(binary),
                "mcp",
                "--config",
                str(config),
                "--repo",
                str(repo),
                "--subscribe-interval-ms",
                "1000",
                "--max-subscriptions",
                "4",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env={**os.environ, **(env or {})},
        )
        self.pending: list[dict] = []
        self.events: queue.Queue[dict | BaseException] = queue.Queue()
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self) -> None:
        assert self.proc.stdout is not None
        try:
            for line in self.proc.stdout:
                self.events.put(json.loads(line))
        except BaseException as exc:  # pragma: no cover - failure path only
            self.events.put(exc)

    def _read_stderr(self) -> None:
        assert self.proc.stderr is not None
        for _ in self.proc.stderr:
            pass

    def send(self, message: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()

    def drain_queued_events(self) -> None:
        while True:
            try:
                item = self.events.get_nowait()
            except queue.Empty:
                return
            if isinstance(item, BaseException):
                raise item
            self.pending.append(item)

    def _take_matching(self, predicate, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            self.drain_queued_events()
            for index, item in enumerate(self.pending):
                if predicate(item):
                    return self.pending.pop(index)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for MCP message; pending={self.pending!r}")
            item = self.events.get(timeout=remaining)
            if isinstance(item, BaseException):
                raise item
            self.pending.append(item)

    def reply(self, request_id: int, timeout: float = 10.0) -> dict:
        return self._take_matching(lambda item: item.get("id") == request_id, timeout)

    def updated_count(self, uri: str) -> int:
        self.drain_queued_events()
        return sum(
            1
            for item in self.pending
            if item.get("method") == "notifications/resources/updated"
            and item.get("params", {}).get("uri") == uri
        )

    def wait_updated(self, uri: str, timeout: float) -> dict:
        return self._take_matching(
            lambda item: item.get("method") == "notifications/resources/updated"
            and item.get("params", {}).get("uri") == uri,
            timeout,
        )

    def wait_method(self, method: str, timeout: float) -> dict:
        return self._take_matching(lambda item: item.get("method") == method, timeout)

    def close(self) -> None:
        if self.proc.stdin is not None:
            self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
            raise
        if self.proc.returncode != 0:
            raise RuntimeError(f"walgit mcp exited {self.proc.returncode}")


def git(*args: str, cwd: Path | None = None) -> str:
    out = subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return out.stdout


def pid_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def walgit(binary: Path, config: Path, args: list[str], cwd: Path) -> str:
    out = subprocess.run(
        [str(binary), "--config", str(config), *args],
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return out.stdout


def commit_and_push(repo: Path, value: str) -> None:
    (repo / "value.txt").write_text(value, encoding="utf-8")
    git("add", "value.txt", cwd=repo)
    git("commit", "-m", f"change {value}", cwd=repo)
    git("push", "origin", "main", cwd=repo)


def commit_and_push_url(repo: Path, url: str, value: str) -> None:
    (repo / "wal.txt").write_text(value, encoding="utf-8")
    git("add", "wal.txt", cwd=repo)
    git("commit", "-m", f"wal change {value}", cwd=repo)
    git("-c", "http.sslVerify=false", "push", url, "main:main", cwd=repo)


def collab_entry(
    binary: Path,
    config: Path,
    writer: Path,
    key: Path,
    *,
    kind: str,
    thread: str,
    parent: str,
    body: dict,
) -> tuple[str, str]:
    out = walgit(
        binary,
        config,
        [
            "collab",
            "entry",
            "--repo",
            str(writer),
            "--kind",
            kind,
            "--id",
            thread,
            "--actor",
            "smoke",
            "--parent",
            parent,
            "--body",
            json.dumps(body, ensure_ascii=False),
            "--key",
            str(key),
            "--push",
            "origin",
        ],
        writer,
    )
    line = next(line for line in reversed(out.splitlines()) if line.strip())
    ref, oid = line.split()
    return ref, oid


def main() -> int:
    if len(sys.argv) not in (3, 4):
        print(
            "usage: mcp-subscribe-smoke.py <walgit-binary> <walgit.toml> [wal-repo]",
            file=sys.stderr,
        )
        return 2
    binary = Path(sys.argv[1]).resolve()
    config = Path(sys.argv[2]).resolve()
    wal_repo = sys.argv[3] if len(sys.argv) == 4 else None
    server_url = os.environ.get("WALGIT_URL")
    if not binary.is_file():
        raise FileNotFoundError(binary)
    if not config.is_file():
        raise FileNotFoundError(config)
    if wal_repo is not None and not server_url:
        raise RuntimeError("WAL smoke requires WALGIT_URL")

    with tempfile.TemporaryDirectory(prefix="walgit-mcp-subscribe-") as tmp:
        root = Path(tmp)
        remote = root / "owner" / "repo.git"
        checkout = root / "checkout"
        writer = root / "writer"
        remote.parent.mkdir(parents=True)
        git("init", "--bare", str(remote))
        git("init", "-b", "main", str(checkout))
        git("config", "user.name", "MCP Smoke", cwd=checkout)
        git("config", "user.email", "mcp-smoke@example.invalid", cwd=checkout)
        (checkout / "value.txt").write_text("one\n", encoding="utf-8")
        git("add", "value.txt", cwd=checkout)
        git("commit", "-m", "initial", cwd=checkout)
        git("remote", "add", "origin", str(remote), cwd=checkout)
        git("push", "-u", "origin", "main", cwd=checkout)

        git("clone", str(remote), str(writer))
        git("config", "user.name", "MCP Smoke Writer", cwd=writer)
        git("config", "user.email", "mcp-smoke-writer@example.invalid", cwd=writer)
        key = root / "smoke.ed25519"
        key.write_text(secrets.token_hex(32), encoding="ascii")
        os.chmod(key, 0o600)
        walgit(
            binary,
            config,
            [
                "collab",
                "principal-register",
                "--repo",
                str(writer),
                "--principal",
                "smoke",
                "--key",
                str(key),
                "--push",
                "origin",
            ],
            writer,
        )

        fakebin = root / "fakebin"
        fakebin.mkdir()
        trigger = root / "fake-git-trigger"
        fake_git_pids = root / "fake-git-pids"
        real_git = shutil.which("git")
        assert real_git is not None
        fake_git = fakebin / "git"
        fake_git.write_text(
            "#!/bin/sh\n"
            "if [ -f \"$WALGIT_FAKE_GIT_TRIGGER\" ]; then\n"
            "  echo \"$$\" >> \"$WALGIT_FAKE_GIT_PIDS\"\n"
            "  sleep 300 &\n"
            "  child=$!\n"
            "  echo \"$child\" >> \"$WALGIT_FAKE_GIT_PIDS\"\n"
            "  wait \"$child\"\n"
            "else\n"
            "  exec \"$WALGIT_REAL_GIT\" \"$@\"\n"
            "fi\n",
            encoding="utf-8",
        )
        os.chmod(fake_git, 0o755)
        mcp = Mcp(
            binary,
            config,
            checkout,
            {
                "PATH": f"{fakebin}{os.pathsep}{os.environ.get('PATH', '')}",
                "WALGIT_REAL_GIT": real_git,
                "WALGIT_FAKE_GIT_TRIGGER": str(trigger),
                "WALGIT_FAKE_GIT_PIDS": str(fake_git_pids),
            },
        )
        refs_uri = "walgit://refs/owner/repo"
        board_uri = "walgit://collab/board/owner/repo"
        thread_uri = "walgit://collab/thread/owner/repo/smoke-thread"
        try:
            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18"},
                }
            )
            init = mcp.reply(1)
            assert init["result"]["capabilities"]["resources"]["subscribe"] is True, init

            mcp.send({"jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {}})
            listed = mcp.reply(2)
            uris = [item["uri"] for item in listed["result"]["resources"]]
            assert refs_uri in uris, listed
            assert board_uri in uris, listed

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "resources/read",
                    "params": {"uri": refs_uri},
                }
            )
            read = mcp.reply(3)
            content = read["result"]["contents"][0]
            payload = json.loads(content["text"])
            assert payload["_meta"]["version"] == content["_meta"]["version"], read

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "resources/subscribe",
                    "params": {"uri": refs_uri},
                }
            )
            assert mcp.reply(4)["result"] == {}
            time.sleep(1.4)
            assert mcp.updated_count(refs_uri) == 0, mcp.pending

            commit_and_push(checkout, "two\n")
            mcp.wait_updated(refs_uri, 6.0)
            time.sleep(1.3)
            assert mcp.updated_count(refs_uri) == 0, f"duplicate update: {mcp.pending!r}"

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "resources/unsubscribe",
                    "params": {"uri": refs_uri},
                }
            )
            assert mcp.reply(5)["result"] == {}
            commit_and_push(checkout, "three\n")
            time.sleep(1.5)
            assert mcp.updated_count(refs_uri) == 0, mcp.pending

            # A second checkout creates the collab thread and pushes it. The
            # MCP checkout has not fetched it when the subscription starts.
            root_ref, root_oid = collab_entry(
                binary,
                config,
                writer,
                key,
                kind="issue",
                thread="smoke-thread",
                parent="",
                body={
                    "title": "smoke thread",
                    "status": "in-progress",
                    "owner": "smoke",
                    "worktree": "writer",
                    "branch": "main",
                },
            )
            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "resources/subscribe",
                    "params": {"uri": board_uri},
                }
            )
            assert mcp.reply(6)["result"] == {}
            time.sleep(1.4)
            assert mcp.updated_count(board_uri) == 0, mcp.pending

            comment_ref, comment_oid = collab_entry(
                binary,
                config,
                writer,
                key,
                kind="comment",
                thread="smoke-thread",
                parent=root_oid,
                body={"text": "cross-checkout board update"},
            )
            mcp.wait_updated(board_uri, 8.0)

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "resources/read",
                    "params": {"uri": board_uri},
                }
            )
            board_content = mcp.reply(7)["result"]["contents"][0]
            board_payload = json.loads(board_content["text"])
            assert board_payload["_meta"]["version"] == board_content["_meta"]["version"]
            board = board_payload["data"]["board"]
            card = next(
                card
                for column in board["columns"]
                for card in column["cards"]
                if card["id"] == "smoke-thread"
            )
            assert card["entries"] >= 2, card

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 8,
                    "method": "resources/unsubscribe",
                    "params": {"uri": board_uri},
                }
            )
            assert mcp.reply(8)["result"] == {}

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 9,
                    "method": "resources/subscribe",
                    "params": {"uri": thread_uri},
                }
            )
            assert mcp.reply(9)["result"] == {}
            time.sleep(1.4)
            assert mcp.updated_count(thread_uri) == 0, mcp.pending

            second_ref, second_oid = collab_entry(
                binary,
                config,
                writer,
                key,
                kind="comment",
                thread="smoke-thread",
                parent=comment_oid,
                body={"text": "thread head update"},
            )
            mcp.wait_updated(thread_uri, 8.0)

            # Delete the whole thread from the writer and push the deletions:
            # the shared ref-level puller prunes them locally, so the thread
            # resource disappears and the subscription reports list_changed.
            refs = (root_ref, comment_ref, second_ref)
            for ref in refs:
                git("update-ref", "-d", ref, cwd=writer)
            git(
                "push",
                "origin",
                *[f":{ref}" for ref in refs],
                cwd=writer,
            )
            mcp.wait_method("notifications/resources/list_changed", 10.0)

            # Real backoff/failure-threshold path: keep the board subscription
            # alive, make its shared collab puller fail, and require the
            # subscription to report the failure after three consecutive errors.
            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 10,
                    "method": "resources/subscribe",
                    "params": {"uri": board_uri},
                }
            )
            assert mcp.reply(10)["result"] == {}
            git("remote", "set-url", "origin", str(root / "missing-origin"), cwd=checkout)
            failure = mcp.wait_method("notifications/message", 30.0)
            assert failure["params"]["data"]["uri"] == board_uri, failure
            assert failure["params"]["level"] == "error", failure
            git("remote", "set-url", "origin", str(remote), cwd=checkout)
            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 11,
                    "method": "resources/unsubscribe",
                    "params": {"uri": board_uri},
                }
            )
            assert mcp.reply(11)["result"] == {}

            if wal_repo is not None:
                assert server_url is not None
                walgit(binary, config, ["repo", "create", wal_repo], root)
                remote_url = f"{server_url.rstrip('/')}/{wal_repo}.git"
                commit_and_push_url(checkout, remote_url, "one")
                wal_uri = f"walgit://wal/{wal_repo}?from=0"
                mcp.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 12,
                        "method": "resources/subscribe",
                        "params": {"uri": wal_uri},
                    }
                )
                assert mcp.reply(12)["result"] == {}
                time.sleep(1.4)
                assert mcp.updated_count(wal_uri) == 0, mcp.pending

                commit_and_push_url(checkout, remote_url, "two")
                mcp.wait_updated(wal_uri, 8.0)

                mcp.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 13,
                        "method": "resources/read",
                        "params": {"uri": wal_uri},
                    }
                )
                wal_payload = json.loads(mcp.reply(13)["result"]["contents"][0]["text"])
                assert wal_payload["data"]["head_seq"] > 0, wal_payload
                assert wal_payload["data"]["entries"], wal_payload

                mcp.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 14,
                        "method": "resources/unsubscribe",
                        "params": {"uri": wal_uri},
                    }
                )
                assert mcp.reply(14)["result"] == {}

            # Leave one subscription active, make its next probe a long-lived
            # git child, then SIGTERM the MCP process: shutdown must cancel the
            # poller, kill the whole child tree, and exit promptly.
            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 15,
                    "method": "resources/subscribe",
                    "params": {"uri": refs_uri},
                }
            )
            assert mcp.reply(15)["result"] == {}
            trigger.write_text("", encoding="utf-8")
            for _ in range(100):
                if fake_git_pids.exists() and len(fake_git_pids.read_text().splitlines()) >= 2:
                    break
                time.sleep(0.05)
            else:
                raise TimeoutError("fake git probe did not start")
            probe_pids = [int(pid) for pid in fake_git_pids.read_text().splitlines()]
            mcp.proc.send_signal(signal.SIGTERM)
            mcp.proc.wait(timeout=10)
            assert mcp.proc.returncode == 0, mcp.proc.returncode
            for pid in probe_pids:
                for _ in range(100):
                    if not pid_exists(pid):
                        break
                    time.sleep(0.02)
                assert not pid_exists(pid), f"probe process {pid} survived SIGTERM"
        finally:
            mcp.close()
    print("MCP subscription smoke OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
