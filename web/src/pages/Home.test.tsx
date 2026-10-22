import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { HomeBody } from "./Home";

vi.mock("../i18n", () => ({
  useI18n: () => ({ t: (k: string) => k, lang: "en" }),
}));


afterEach(cleanup);

describe("Home (2026-10-23 redesign)", () => {
  it("states what walgit is in one sentence and leads into SKILL.md", () => {
    render(<HomeBody live={null} />);
    expect(screen.getByText("home.oneline")).toBeTruthy();
    const skill = screen.getByText("SKILL.md");
    expect(skill.closest("a")?.getAttribute("href")).toBe("/SKILL.md");
  });

  it("renders the five role prompts with copyable bodies", () => {
    render(<HomeBody live={{ owner: "acme", repo: "r2demo" }} />);
    expect(screen.getByText("home.roles.t")).toBeTruthy();
    for (const role of ["leader", "dev", "reviewer", "tester", "devops"]) {
      expect(screen.getByText(`home.role.${role}.t`)).toBeTruthy();
      expect(screen.getByText(`home.role.${role}.body`)).toBeTruthy();
    }
    // Live link to the first repo's collab guide when the host has a repo.
    expect(screen.getByText("home.live").closest("a")?.getAttribute("href")).toBe(
      "/acme/r2demo/collab/guide",
    );
  });
});
