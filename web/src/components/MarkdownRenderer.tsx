import ReactMarkdown, { type UrlTransform } from "react-markdown";
import remarkGfm from "remark-gfm";
import { resolveMarkdownTarget, type MarkdownBase } from "../markdown-paths";

const plugins = [remarkGfm];

export default function MarkdownRenderer({
  source,
  base,
}: {
  source: string;
  /** Set for repository markdown: links/images are relative to *this* file. */
  base?: MarkdownBase;
}) {
  // `urlTransform` runs for every URL react-markdown emits, and the hast node
  // tells us whether it is an image (raw bytes) or a link (blob/tree route).
  const urlTransform: UrlTransform | undefined = base
    ? (url, _key, node) =>
        resolveMarkdownTarget(url, base, node.tagName === "img" ? "image" : "link")
    : undefined;
  return (
    <ReactMarkdown remarkPlugins={plugins} urlTransform={urlTransform}>
      {source}
    </ReactMarkdown>
  );
}
