import { describe, expect, it } from "vitest";
import { blobKind } from "./blob-kind";

describe("blobKind", () => {
  it("picks a viewer per extension", () => {
    expect(blobKind("content/pic.PNG")).toBe("image");
    expect(blobKind("a/b/clip.webm")).toBe("video");
    expect(blobKind("song.flac")).toBe("audio");
    expect(blobKind("docs/spec.pdf")).toBe("pdf");
    expect(blobKind("site/index.htm")).toBe("html");
    expect(blobKind("README.md")).toBe("markdown");
  });

  it("falls back to text, not to a wrong viewer", () => {
    // A dotfile has no extension; neither does a Makefile — and an unknown
    // extension is decided by the API's text check, not here.
    expect(blobKind(".gitignore")).toBe("text");
    expect(blobKind("Makefile")).toBe("text");
    expect(blobKind("data.bin")).toBe("text");
    expect(blobKind("trailing.")).toBe("text");
  });
});
