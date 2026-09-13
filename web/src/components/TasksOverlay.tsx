import { useEffect, useRef, useState } from "react";
import { api, type TaskRecord } from "../api";
import { reportError } from "../data";
import { useI18n, type I18nKey, type TFunc } from "../i18n";
import { taskErrorText } from "../taskErrors";

/** Poll cadence while something runs / when idle (API.md §2c). */
const BUSY_MS = 1500;
const IDLE_MS = 15000;
/** How long a finished task stays listed in the dropdown. */
const LINGER_MS = 20000;

/** Task kinds are data slugs emitted by the server; translate the known ones
    at render time and pass unknown future kinds through prettified. */
const KIND_KEYS: Record<string, I18nKey> = {
  materialize: "tasks.kind.materialize",
  "remote-index": "tasks.kind.remote-index",
  prewarm: "tasks.kind.prewarm",
  checkpoint: "tasks.kind.checkpoint",
  bundle: "tasks.kind.bundle",
  compact: "tasks.kind.compact",
  repair: "tasks.kind.repair",
  "base-rebuild": "tasks.kind.base-rebuild",
  "rev-index": "tasks.kind.rev-index",
  fsck: "tasks.kind.fsck",
  gc: "tasks.kind.gc",
  follow: "tasks.kind.follow",
};

function kindLabel(t: TFunc, kind: string): string {
  const key = KIND_KEYS[kind];
  return key ? t(key) : kind.replace(/[-_]/g, " ");
}

/**
 * What the serving instance is doing to this repository right now
 * (materializing packs, indexing remote packs, checkpoint, bundle…), as a
 * compact indicator in the repo header: spinner + the name of the job (+N
 * more) + its percent. Clicking it opens a dropdown with every running task,
 * its latest progress, and the tasks that just finished. Polls `…/tasks`
 * fast while anything runs, slowly otherwise. Errors go to the tray.
 *
 * Requests are routed to a random instance, so a task id disappearing from
 * `running` only means "finished" when the same instance answered (or the
 * task shows up in `recent` with a result) — never when we simply landed on
 * another instance.
 */
export function TasksOverlay({ repo }: { repo: string }) {
  const { t } = useI18n();
  const [running, setRunning] = useState<TaskRecord[]>([]);
  const [justDone, setJustDone] = useState<TaskRecord[]>([]);
  const [host, setHost] = useState("");
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    let timer = 0;
    let seenHost = "";
    let seen = new Map<string, TaskRecord>();
    const tick = async () => {
      try {
        const res = await api.tasks(repo);
        if (!alive) return;
        setHost(res.hostname);
        const now = new Map(res.running.map((r) => [r.id, r]));
        const done: TaskRecord[] = [];
        for (const [id, prev] of seen) {
          if (now.has(id)) continue;
          const rec = res.recent.find((r) => r.id === id);
          if (rec) done.push(rec);
          else if (res.hostname === seenHost) done.push({ ...prev, ok: true, finished: prev.finished ?? new Date().toISOString(), summary: prev.summary || t("tasks.done") });
          // else: a different instance answered; keep waiting for the owner.
        }
        for (const rec of done)
          if (rec.ok === false)
            // 任务错误按已知模式翻译(issue #117)。
            reportError(new Error(taskErrorText(t, rec.summary).text), `${kindLabel(t, rec.kind)} task`);
        seenHost = res.hostname;
        seen = now;
        setRunning(res.running);
        if (done.length) {
          const ids = new Set(done.map((d) => d.id));
          setJustDone((d) => [...d.filter((x) => !ids.has(x.id)), ...done].slice(-5));
          setTimeout(() => alive && setJustDone((d) => d.filter((x) => !ids.has(x.id))), LINGER_MS);
        }
        timer = window.setTimeout(tick, res.running.length ? BUSY_MS : IDLE_MS);
      } catch (e) {
        if (!alive) return;
        // A 404 means "no such repo here"; anything else is worth a line in the tray, once.
        if ((e as { status?: number }).status !== 404) reportError(e, "tasks");
        timer = window.setTimeout(tick, IDLE_MS);
      }
    };
    void tick();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [repo, t]);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Nothing to show: render nothing (and close a stale dropdown).
  useEffect(() => {
    if (running.length === 0 && justDone.length === 0) setOpen(false);
  }, [running.length, justDone.length]);
  if (running.length === 0 && justDone.length === 0) return null;

  // The headline task: the one with progress, else the newest running, else the latest finished.
  const head = running.find((x) => x.progress) ?? running[0] ?? justDone[justDone.length - 1];
  if (!head) return null;
  const others = running.length > 1 ? running.length - 1 : 0;
  const pct = percentOf(head);
  const failed = justDone.some((x) => x.ok === false);

  return (
    <div className="tasks-indicator" ref={ref}>
      <button
        type="button"
        className={`tasks-pill ${running.length ? "busy" : "idle"} ${failed ? "failed" : ""}`}
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-haspopup="true"
        title={running.length ? t("tasks.title.running", { n: running.length }) : t("tasks.title.finished")}
      >
        {running.length ? <span className="spinner" aria-hidden /> : <span className={`dot ${failed ? "failed" : "ok"}`} aria-hidden />}
        <span className="task-kind">{kindLabel(t, head.kind)}</span>
        {others > 0 && <span className="muted">+{others}</span>}
        {pct !== undefined && <span className="muted tabular">{pct.toFixed(0)}%</span>}
        <span className="caret" aria-hidden>
          ▾
        </span>
      </button>
      {open && (
        <output className="tasks-pop" aria-live="polite">
          {running.length > 0 && (
            <div className="tasks-section">
              <div className="tasks-title muted small">{t("tasks.section.running")}</div>
              {running.map((task) => (
                <TaskLine key={task.id} task={task} />
              ))}
            </div>
          )}
          {justDone.length > 0 && (
            <div className="tasks-section">
              <div className="tasks-title muted small">{t("tasks.section.finished")}</div>
              {justDone.toReversed().map((task) => (
                <div key={task.id} className={`task done ${task.ok === false ? "failed" : ""}`}>
                  <span className={`dot ${task.ok === false ? "failed" : "ok"}`} aria-hidden />
                  <span className="task-kind">{kindLabel(t, task.kind)}</span>
                  <span className="task-text">{task.summary}</span>
                  <span className="muted small tabular">{fmtSecs(task.elapsed_ms)}</span>
                </div>
              ))}
            </div>
          )}
          {host && <div className="muted small task-host">{t("tasks.instance", { host: host.slice(0, 8) })}</div>}
        </output>
      )}
    </div>
  );
}

function percentOf(t: TaskRecord | undefined): number | undefined {
  const p = t?.progress;
  if (!p) return undefined;
  const v = p.percent ?? (p.total ? (100 * p.done) / p.total : undefined);
  return v === undefined ? undefined : Math.min(100, Math.max(0, v));
}

function fmtSecs(ms: number): string {
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s - m * 60)}s`;
}

function TaskLine({ task }: { task: TaskRecord }) {
  const { t } = useI18n();
  const p = task.progress;
  const pct = percentOf(task);
  const last = task.log_tail.at(-1);
  return (
    <div className="task running">
      <div className="task-row">
        <span className="spinner" aria-hidden />
        <span className="task-kind">{kindLabel(t, task.kind)}</span>
        <span className="task-text">{p?.label ?? last ?? task.summary ?? t("tasks.working")}</span>
        {pct !== undefined && <span className="muted small tabular">{pct.toFixed(0)}%</span>}
        <span className="muted small tabular">{fmtSecs(task.elapsed_ms)}</span>
      </div>
      {pct !== undefined && (
        <span className="activity-bar" aria-hidden>
          <span style={{ width: `${pct.toFixed(1)}%` }} />
        </span>
      )}
    </div>
  );
}
