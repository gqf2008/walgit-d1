#!/usr/bin/env python3
"""End-to-end smoke for `walgit mcp` resource subscriptions.

The test uses a local bare repository as `origin`, so it exercises the same
client-side pull lane an MCP host uses without needing a server or bucket.
"""

from __future__ import annotations

import json
import queue
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


class Mcp:
    def __init__(self, binary: Path, config: Path, repo: Path) -> None:
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

    def _take_matching(self, predicate, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        while True:
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


def commit_and_push(repo: Path, value: str) -> None:
    (repo / "value.txt").write_text(value, encoding="utf-8")
    git("add", "value.txt", cwd=repo)
    git("commit", "-m", f"change {value}", cwd=repo)
    git("push", "origin", "main", cwd=repo)


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: mcp-subscribe-smoke.py <walgit-binary> <walgit.toml>", file=sys.stderr)
        return 2
    binary = Path(sys.argv[1]).resolve()
    config = Path(sys.argv[2]).resolve()
    if not binary.is_file():
        raise FileNotFoundError(binary)
    if not config.is_file():
        raise FileNotFoundError(config)

    with tempfile.TemporaryDirectory(prefix="walgit-mcp-subscribe-") as tmp:
        root = Path(tmp)
        remote = root / "owner" / "repo.git"
        checkout = root / "checkout"
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

        mcp = Mcp(binary, config, checkout)
        uri = "walgit://refs/owner/repo"
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
            assert uri in uris, listed

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "resources/read",
                    "params": {"uri": uri},
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
                    "params": {"uri": uri},
                }
            )
            assert mcp.reply(4)["result"] == {}
            time.sleep(1.4)
            assert mcp.updated_count(uri) == 0, mcp.pending

            commit_and_push(checkout, "two\n")
            mcp.wait_updated(uri, 6.0)
            time.sleep(1.3)
            assert mcp.updated_count(uri) == 0, f"duplicate update: {mcp.pending!r}"

            mcp.send(
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "resources/unsubscribe",
                    "params": {"uri": uri},
                }
            )
            assert mcp.reply(5)["result"] == {}
            commit_and_push(checkout, "three\n")
            time.sleep(1.5)
            assert mcp.updated_count(uri) == 0, mcp.pending
        finally:
            mcp.close()
    print("MCP subscription smoke OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
