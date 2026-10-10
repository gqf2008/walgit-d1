import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import type { StoreSettings } from "../api";
import { api } from "../api";
import { SetupPage } from "./SetupPage";

vi.mock("../api", () => {
  class ApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.status = status;
    }
  }
  return {
    ApiError,
    api: {
      store: { get: vi.fn(), test: vi.fn(), save: vi.fn() },
      setAdminToken: vi.fn(),
    },
  };
});
vi.mock("../i18n", () => ({ useI18n: () => ({ t: (k: string) => k }) }));

const snapshot: StoreSettings = {
  backend: "s3",
  bucket: "bucket",
  prefix: "",
  endpoint: "https://account.r2.cloudflarestorage.com",
  region: "auto",
  force_path_style: true,
  has_access_key: true,
  has_secret_key: true,
  can_save: true,
};

beforeEach(() => {
  vi.mocked(api.store.get).mockReset();
  vi.mocked(api.setAdminToken).mockReset();
  // Unconfigured probes 404; the configured editor probe is on `api.store`.
  globalThis.fetch = vi.fn(
    async () => new Response(null, { status: 404 }),
  ) as unknown as typeof fetch;
});
afterEach(cleanup);

describe("SetupPage in token mode (#127 follow-up)", () => {
  it("asks for an admin token and opens the storage editor once one works", async () => {
    vi.mocked(api.store.get)
      .mockRejectedValueOnce(new Error("401 unauthorized"))
      .mockResolvedValueOnce(snapshot);

    render(
      <MemoryRouter>
        <SetupPage />
      </MemoryRouter>,
    );

    // Not an admin: the editor is refused and the token prompt is shown.
    const input = await screen.findByPlaceholderText("wgt_… / static token");
    fireEvent.change(input, { target: { value: "wgt_admin" } });
    fireEvent.click(screen.getByText("store.token.submit"));

    await waitFor(() => expect(vi.mocked(api.setAdminToken)).toHaveBeenCalledWith("wgt_admin"));
    // The editor (its heading) replaces the prompt; the save button is present.
    expect(await screen.findByText("store.title")).toBeTruthy();
    expect(vi.mocked(api.store.get)).toHaveBeenCalledTimes(2);
  });

  it("clears a rejected token so it cannot shadow the session lane", async () => {
    vi.mocked(api.store.get).mockRejectedValue(new Error("403 forbidden"));

    render(
      <MemoryRouter>
        <SetupPage />
      </MemoryRouter>,
    );
    const input = await screen.findByPlaceholderText("wgt_… / static token");
    fireEvent.change(input, { target: { value: "bad" } });
    fireEvent.click(screen.getByText("store.token.submit"));

    await waitFor(() => expect(vi.mocked(api.setAdminToken)).toHaveBeenLastCalledWith(null));
    expect(screen.getByText("store.token.submit")).toBeTruthy();
  });
});
