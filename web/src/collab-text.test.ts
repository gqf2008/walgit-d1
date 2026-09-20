import { describe, expect, it } from "vitest";

import { collabEntryText } from "./collab-text";

/**
 * Mirrored with the Rust test `card_prose_is_the_thread_page_field_walk`
 * (crates/walgit-wal/src/collab.rs): same table, same order — the board card's
 * prose and the thread page's body must be the same extraction (D1 §8.2).
 */
describe("collab entry prose (D1 §8.2)", () => {
  it("is the first non-empty of text/body/note/message/summary, trimmed", () => {
    expect(collabEntryText({})).toBe("");
    expect(collabEntryText({ text: "web" })).toBe("web");
    expect(collabEntryText({ text: "", body: "cli" })).toBe("cli");
    expect(collabEntryText({ text: "", body: "", note: "rev" })).toBe("rev");
    expect(collabEntryText({ message: "patch", summary: "sum" })).toBe("patch");
    expect(collabEntryText({ text: "  trimmed  " })).toBe("trimmed");
    // Non-string fields never win, even when non-empty.
    expect(collabEntryText({ text: 7, body: "cli" })).toBe("cli");
  });
});
