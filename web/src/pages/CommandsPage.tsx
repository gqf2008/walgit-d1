import { useRef, useState, type ReactNode } from "react";
import { Box } from "../components/Layout";
import { useI18n } from "../i18n";

/**
 * 「命令速查」(/commands, D61): the agent-facing CLI cheat sheet, rendered in
 * the product so it is always one click away. Data-free: it is a projection of
 * the CLI surface, not of any repository. Every command block copies on click
 * (clipboard first; selection as the fallback).
 *
 * Update it with the CLI in the same change when a listed command changes; the
 * normative text stays in web/SKILL.md §0a–§0e and skills/walgit/SKILL.md §5.
 */

function CommandRow({ cmd, note }: { cmd: string; note?: string }) {
  const { t } = useI18n();
  const code = useRef<HTMLElement>(null);
  const timer = useRef<number | undefined>(undefined);
  const [state, setState] = useState<"idle" | "copied" | "selected">("idle");

  function flash(next: "copied" | "selected") {
    setState(next);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState("idle"), 1400);
  }

  function select() {
    const node = code.current;
    const selection = window.getSelection();
    if (!node || !selection) return;
    const range = document.createRange();
    range.selectNodeContents(node);
    selection.removeAllRanges();
    selection.addRange(range);
  }

  async function copy() {
    try {
      await navigator.clipboard.writeText(cmd);
      flash("copied");
    } catch {
      select();
      flash("selected");
    }
  }

  const label =
    state === "copied" ? t("cmds.copied") : state === "selected" ? t("cmds.selected") : t("cmds.copy");
  return (
    <>
      <div className="cmds-row">
        <code ref={code} className="mono">
          {cmd}
        </code>
        <button type="button" className="cmds-copy" onClick={copy} aria-label={`${t("cmds.copy")}: ${cmd}`}>
          {label}
        </button>
      </div>
      {note && <p className="cmds-note">{note}</p>}
    </>
  );
}

function Section({ title, lead, children }: { title: string; lead: string; children: ReactNode }) {
  return (
    <Box title={title}>
      <div className="pad">
        <p className="cmds-lead">{lead}</p>
        {children}
      </div>
    </Box>
  );
}

function Fact({ title, body }: { title: string; body: string }) {
  return (
    <div className="cmds-fact">
      <strong>{title}</strong>
      <span>{body}</span>
    </div>
  );
}

export function CommandsPage() {
  const { t } = useI18n();
  return (
    <>
      <Box title={t("cmds.title")}>
        <div className="pad">
          <p className="cmds-lead">{t("cmds.lede")}</p>
          <div className="cmds-facts">
            <Fact title={t("cmds.f1.t")} body={t("cmds.f1.b")} />
            <Fact title={t("cmds.f2.t")} body={t("cmds.f2.b")} />
            <Fact title={t("cmds.f3.t")} body={t("cmds.f3.b")} />
          </div>
        </div>
      </Box>

      <Section title={t("cmds.s1.t")} lead={t("cmds.s1.lead")}>
        <CommandRow cmd="git for-each-ref refs/collab/meta/principals" />
        <CommandRow
          cmd='walgit collab join --repo "$checkout" --principal <proj>-worker-2 --push origin'
          note={t("cmds.s1.join")}
        />
        <CommandRow cmd="git fetch origin '+refs/collab/inbox/*:refs/collab/inbox/*' '+refs/collab/meta/*:refs/collab/meta/*'" />
        <CommandRow cmd="walgit collab board" note={t("cmds.s1.board")} />
      </Section>

      <Section title={t("cmds.s2.t")} lead={t("cmds.s2.lead")}>
        <CommandRow cmd="walgit collab board" note={t("cmds.s2.board")} />
        <CommandRow cmd="walgit collab thread <id>" note={t("cmds.s2.thread")} />
        <CommandRow cmd="walgit collab thread-heads" />
        <CommandRow cmd="walgit collab pr <id>" note={t("cmds.s2.pr")} />
        <CommandRow cmd="walgit collab report --remote origin" note={t("cmds.s2.report")} />
        <CommandRow cmd="walgit collab ls" />
      </Section>

      <Section title={t("cmds.s3.t")} lead={t("cmds.s3.lead")}>
        <CommandRow
          cmd='walgit collab claim --id <card> --worktree <wt> --branch <br> --work "<plan>" --session <id> --push origin'
          note={t("cmds.s3.claim")}
        />
        <div className="cmds-table-wrap">
          <table className="cmds-table">
            <thead>
              <tr>
                <th>{t("cmds.s3.exit.col1")}</th>
                <th>{t("cmds.s3.exit.col2")}</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td>
                  <code className="mono">0</code>
                </td>
                <td>{t("cmds.s3.exit0")}</td>
              </tr>
              <tr>
                <td>
                  <code className="mono">2</code>
                </td>
                <td>{t("cmds.s3.exit2")}</td>
              </tr>
              <tr>
                <td>
                  <code className="mono">1</code>
                </td>
                <td>{t("cmds.s3.exit1")}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <p className="cmds-note">{t("cmds.s3.entries")}</p>
        <CommandRow cmd={`walgit collab entry --kind issue --id <thread> --body '{"title":"…","body":"…"}' --push origin`} />
        <CommandRow cmd={`walgit collab entry --kind comment --id <thread> --parent <oid> --body '{"note":"…"}' --push origin`} />
        <CommandRow
          cmd={`walgit collab entry --kind patch --id <thread> --parent <oid> --base refs/heads/main --head refs/heads/<br> --body '{"note":"…"}' --push origin`}
          note={t("cmds.s3.review")}
        />
        <CommandRow
          cmd={`walgit collab entry --kind status --id <card> --parent <oid> --body '{"status":"needs-review","owner":"<p>","worktree":"<wt>","branch":"<br>"}' --push origin`}
          note={t("cmds.s3.close")}
        />
        <CommandRow cmd="walgit collab gc --actor <principal> --push origin [--truncate]" note={t("cmds.s3.gc")} />
      </Section>

      <Section title={t("cmds.s4.t")} lead={t("cmds.s4.lead")}>
        <CommandRow cmd="walgit repo refs <owner/repo> [branches|tags|all|collab]" />
        <CommandRow cmd="walgit repo resolve <owner/repo> <rev>" />
        <CommandRow cmd="walgit repo tree <owner/repo> <rev> [path]" />
        <CommandRow cmd="walgit repo blob <owner/repo> <rev> <path> [--raw]" />
        <CommandRow cmd="walgit repo commits <owner/repo> --ref main --n 20 --path <path>" />
        <CommandRow cmd="walgit repo commit <owner/repo> <sha>" />
        <CommandRow cmd="walgit repo diff <owner/repo> <from> <to> [--format patch]" />
        <CommandRow cmd="walgit repo merge-base <owner/repo> <from> <to>" />
        <CommandRow cmd="walgit repo blame <owner/repo> <rev> <path>" />
        <CommandRow cmd="walgit repo archive <owner/repo> <rev> [--out FILE]" />
        <CommandRow cmd="walgit repo overview <owner/repo>" />
        <CommandRow cmd="walgit repo owners [<owner>]" note={t("cmds.s4.owners")} />
      </Section>

      <Section title={t("cmds.s5.t")} lead={t("cmds.s5.lead")}>
        <CommandRow
          cmd='walgit collab watch --repo "$checkout" --remote origin --interval 10 --exec /path/watch-hook.sh'
          note={t("cmds.s5.hook")}
        />
        <CommandRow
          cmd='walgit collab watch --once --repo "$checkout" --remote origin --exec /path/watch-hook.sh'
          note={t("cmds.s5.once")}
        />
        <CommandRow
          cmd='walgit mcp --repo "$checkout" --subscribe-interval-ms 5000 --max-subscriptions 32'
          note={t("cmds.s5.mcp")}
        />
        <CommandRow cmd="walgit mcp --allow-write --key <key>" />
        <CommandRow cmd="walgit://refs/<owner>/<repo>  ·  walgit://collab/board/<owner>/<repo>" note={t("cmds.s5.uris")} />
      </Section>

      <Section title={t("cmds.s6.t")} lead={t("cmds.s6.lead")}>
        <CommandRow cmd="walgit ci validate" />
        <CommandRow cmd="walgit ci run" note={t("cmds.s6.run")} />
        <CommandRow cmd="walgit ci status" />
        <CommandRow cmd="walgit ci log [<run>]" />
        <CommandRow cmd="walgit ci artifacts [<run>] --out <dir>" note={t("cmds.s6.artifacts")} />
      </Section>

      <Section title={t("cmds.s7.t")} lead={t("cmds.s7.lead")}>
        <CommandRow cmd="walgit wal head <owner/repo>" />
        <CommandRow cmd="walgit wal ls <owner/repo>" />
        <CommandRow cmd="walgit wal show <owner/repo> <seq>" />
        <CommandRow cmd="walgit repo tasks <owner/repo> --follow <id>" />
        <CommandRow cmd="walgit repo ops <owner/repo>" />
        <CommandRow cmd="walgit repo op-start <owner/repo> <op> --arg k=v" note={t("cmds.s7.global")} />
      </Section>

      <div className="pad muted">{t("cmds.footer")}</div>
    </>
  );
}
