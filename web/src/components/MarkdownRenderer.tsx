import ReactMarkdown, { defaultUrlTransform, type UrlTransform } from "react-markdown";
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
  // Providing a `urlTransform` *replaces* react-markdown's own protocol filter,
  // so the filter has to be re-applied first: an untrusted blob that links to
  // `javascript:…` must still be neutralised before any of our rewriting runs.
  const urlTransform: UrlTransform | undefined = base
    ? (url, _key, node) => {
        const safe = defaultUrlTransform(url);
        if (safe !== url) return safe;
        return resolveMarkdownTarget(url, base, node.tagName === "img" ? "image" : "link");
      }
    : undefined;
  return (
    <ReactMarkdown remarkPlugins={plugins} urlTransform={urlTransform}>
      {source}
    </ReactMarkdown>
  );
}
