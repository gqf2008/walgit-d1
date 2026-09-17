import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import MarkdownRenderer from "./MarkdownRenderer";

/**
 * The renderer's security boundary (issue #112): entry prose is untrusted
 * (any actor's signed entries), so the markdown pipeline must hold against
 * XSS-shaped input. These cases lock react-markdown's defaults: raw HTML is
 * never parsed, and URLs are protocol-filtered.
 */
describe("MarkdownRenderer (untrusted entry prose — XSS boundary)", () => {
  it("renders common markdown", () => {
    const { container } = render(<MarkdownRenderer source={"**bold** and [link](https://x.example)"} />);
    expect(container.querySelector("strong")?.textContent).toBe("bold");
    expect(container.querySelector("a")?.getAttribute("href")).toBe("https://x.example");
  });

  it("does not render raw HTML", () => {
    const { container } = render(<MarkdownRenderer source={"<script>alert(1)</script>"} />);
    expect(container.querySelector("script")).toBeNull();
    expect(container.textContent).toContain("<script>alert(1)</script>");
  });

  it("does not emit event-handler attributes from raw HTML", () => {
    const { container } = render(<MarkdownRenderer source={'<img src=x onerror="alert(1)">'} />);
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("[onerror]")).toBeNull();
  });

  it("drops javascript: URLs in links", () => {
    const { container } = render(<MarkdownRenderer source={"[x](javascript:alert(1))"} />);
    const href = container.querySelector("a")?.getAttribute("href") ?? "";
    expect(href).not.toContain("javascript:");
  });

  it("drops mixed-case javascript: URLs in links", () => {
    const { container } = render(<MarkdownRenderer source={"[x](JaVaScRiPt:alert(1))"} />);
    const href = container.querySelector("a")?.getAttribute("href") ?? "";
    expect(href).not.toContain("javascript:");
  });

  it("drops data: URLs in links", () => {
    const { container } = render(<MarkdownRenderer source={"[x](data:text/html,<script>1</script>)"} />);
    const href = container.querySelector("a")?.getAttribute("href") ?? "";
    expect(href).not.toContain("data:");
  });

  it("never renders an image with an unsafe src", () => {
    // The invariant: img elements carry no javascript:/data: src (an img may
    // remain, src-less — no request, no execution).
    for (const src of ["javascript:alert(1)", "data:image/png;base64,AAAA"]) {
      const { container } = render(<MarkdownRenderer source={`![x](${src})`} />);
      expect(container.querySelector("img")?.getAttribute("src") ?? null).toBeNull();
    }
  });

  it("renders GFM tables via remark-gfm", () => {
    const { container } = render(<MarkdownRenderer source={"| a | b |\n| - | - |\n| 1 | 2 |"} />);
    expect(container.querySelector("table")).not.toBeNull();
  });

  // The ventures bug: markdown links are relative to the *file*, not the page.
  it("resolves relative links and images against the given base", () => {
    const urls = {
      raw: (rev: string, path: string) => `/r/api/blob/${rev}/${path}?raw`,
      tree: (rev: string, path = "") => `/r/tree/${rev}${path ? "/" + path : ""}`,
      blob: (rev: string, path: string) => `/r/blob/${rev}/${path}`,
    };
    const { container } = render(
      <MarkdownRenderer
        source={"[dir](vdev-driver-ip/README.md) ![pic](pics/a.png)"}
        base={{ ref: "main", dir: "content", urls }}
      />,
    );
    expect(container.querySelector("a")?.getAttribute("href")).toBe(
      "/r/blob/main/content/vdev-driver-ip/README.md",
    );
    expect(container.querySelector("img")?.getAttribute("src")).toBe(
      "/r/api/blob/main/content/pics/a.png?raw",
    );
  });
});
