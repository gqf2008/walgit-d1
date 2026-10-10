import { describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { Layout } from "./Layout";

// The chrome around the page is not under test — only that the storage entry is
// reachable regardless of identity (the reported bug hid it behind `me.admin`).
vi.mock("../i18n", () => ({ useI18n: () => ({ t: (k: string) => k }), LangSwitch: () => null }));
vi.mock("./Loading", () => ({
  RouteBoundary: ({ children }: { children?: ReactNode }) => children,
  TopProgress: () => null,
  useBusy: () => 0,
}));
vi.mock("./ErrorTray", () => ({ ErrorTray: () => null }));
vi.mock("./InstanceFooter", () => ({ InstanceFooter: () => null }));

describe("Layout", () => {
  it("always links to the storage editor, admin or not", () => {
    render(
      <MemoryRouter initialEntries={["/"]}>
        <Routes>
          <Route element={<Layout />}>
            <Route index element={<div>page</div>} />
          </Route>
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.getByText("nav.store").closest("a")?.getAttribute("href")).toBe("/setup");
  });
});
