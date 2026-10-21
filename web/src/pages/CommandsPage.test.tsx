import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { CommandsPage } from "./CommandsPage";

vi.mock("../i18n", () => ({ useI18n: () => ({ t: (k: string) => k }) }));

afterEach(cleanup);

describe("CommandsPage", () => {
  it("renders the workflow sections with copyable command blocks", () => {
    render(<CommandsPage />);
    expect(screen.getByText("cmds.title")).toBeTruthy();
    // The board command appears both in first-contact and in the read section.
    expect(screen.getAllByText("walgit collab board")).toHaveLength(2);
    expect(screen.getByText("walgit collab thread <id>")).toBeTruthy();
    expect(
      screen.getAllByRole("button", { name: /cmds\.copy:/ }).length,
    ).toBeGreaterThan(20);
  });

  it("copies a command to the clipboard and confirms", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    render(<CommandsPage />);
    const row = screen
      .getByText("git for-each-ref refs/collab/meta/principals")
      .closest(".cmds-row");
    expect(row).toBeTruthy();
    fireEvent.click(row!.querySelector("button")!);
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith("git for-each-ref refs/collab/meta/principals"),
    );
    expect(screen.getByText("cmds.copied")).toBeTruthy();
  });
});
