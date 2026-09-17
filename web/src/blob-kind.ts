/** How the blob page presents a file, chosen from its name.
 *
 *  This is a *presentation* decision only. Whether the bytes can be fetched is
 *  the server's call (`?raw` serves any type up to its cap), and whether the
 *  file is text is the API's call (it returns `contents` only for text) — the
 *  viewer must not second-guess either of those with an extension guess.
 */
export type BlobKind = "markdown" | "image" | "video" | "audio" | "pdf" | "html" | "text";

const EXTENSIONS: Record<string, BlobKind> = {
  md: "markdown",
  markdown: "markdown",
  png: "image",
  jpg: "image",
  jpeg: "image",
  gif: "image",
  webp: "image",
  avif: "image",
  bmp: "image",
  ico: "image",
  svg: "image",
  mp4: "video",
  m4v: "video",
  webm: "video",
  ogv: "video",
  mov: "video",
  mp3: "audio",
  wav: "audio",
  ogg: "audio",
  oga: "audio",
  m4a: "audio",
  flac: "audio",
  pdf: "pdf",
  html: "html",
  htm: "html",
};

export function blobKind(path: string): BlobKind {
  const name = path.slice(path.lastIndexOf("/") + 1);
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return "text";
  return EXTENSIONS[name.slice(dot + 1).toLowerCase()] ?? "text";
}
