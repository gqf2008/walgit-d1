/**
 * The D1 entry prose walk (docs/D1_PROTOCOL.md §8.2, issue #112): the first
 * non-empty of `text` / `body` / `note` / `message` / `summary`, trimmed.
 *
 * Mirrors the Rust `entry_prose` in crates/walgit-wal/src/collab.rs. The two
 * sides are pinned to the same table by web/src/collab-text.test.ts and the
 * Rust test `card_prose_is_the_thread_page_field_walk`, so the board card and
 * the thread page cannot drift apart.
 */
export function collabEntryText(body: Record<string, unknown>): string {
  for (const key of ["text", "body", "note", "message", "summary"]) {
    const value = body[key];
    if (typeof value === "string" && value.trim() !== "") return value.trim();
  }
  return "";
}
