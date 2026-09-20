import { describe, expect, it } from "vitest";
import type { CollabBoardColumn } from "../api";
import { filterProjectColumns } from "./CollabProjectsPage";

const columns: CollabBoardColumn[] = [
  {
    name: "open",
    cards: [
      { id: "one", title: "Fix login", owner: "alice", status: "open" },
      { id: "two", title: "Add search", owner: "bob", status: "open" },
    ],
  },
  {
    name: "in-progress",
    cards: [{ id: "three", title: "Fix billing", owner: "alice", status: "in-progress" }],
  },
] as unknown as CollabBoardColumn[];

describe("filterProjectColumns", () => {
  it("filters by status, owner and text without changing order", () => {
    const filtered = filterProjectColumns(columns, "open", "alice", "login");
    expect(filtered.map((column) => column.name)).toEqual(["open"]);
    expect(filtered[0]?.cards.map((card) => card.id)).toEqual(["one"]);
  });

  it("drops columns that become empty", () => {
    expect(filterProjectColumns(columns, "", "nobody", "").length).toBe(0);
  });
});
