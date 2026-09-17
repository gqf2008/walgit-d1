import { Suspense, lazy } from "react";
import type { MarkdownBase } from "../markdown-paths";

// react-markdown + remark-gfm (micromark, mdast, hast…) is ~100 kB gzipped;
// only pay for it when a README/markdown blob is actually rendered.
const Renderer = lazy(() => import("./MarkdownRenderer"));

export function Markdown({ source, base }: { source: string; base?: MarkdownBase }) {
  return (
    <div className="markdown-body">
      <Suspense fallback={<pre className="code-block">{source}</pre>}>
        <Renderer source={source} base={base} />
      </Suspense>
    </div>
  );
}
