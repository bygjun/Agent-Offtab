import { memo, useLayoutEffect, useRef } from 'react';
import { FileText } from 'lucide-react';
import type { Element, Root } from 'hast';
import type { Root as MarkdownRoot, PhrasingContent, Text } from 'mdast';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { graphemes } from '../typed-preview';

const components: Components = {
  span({ node, children, ...props }) {
    const step = node?.properties.dataTypingStep;
    return typeof step === 'number'
      ? <span key={String(node?.properties.dataTypingRevision)} className="answer-typing-fragment"
        style={{ animationDelay: `${step * 10}ms` }}>{children}</span>
      : <span {...props}>{children}</span>;
  },
  a({ href, children }) {
    if (!href || !/^https?:\/\//i.test(href)) {
      return <span className="markdown-reference"><FileText aria-hidden="true" />
        <span className="sr-only">Mac 파일 참조 </span>{children}</span>;
    }
    return <a href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
  },
  img({ alt }) {
    return alt ? <span>{alt}</span> : null;
  },
  pre({ children }) {
    return <pre tabIndex={0}>{children}</pre>;
  },
  table({ children }) {
    return <div className="markdown-table" role="region" aria-label="답변 표" tabIndex={0}>
      <table>{children}</table>
    </div>;
  },
};

export function remarkQuotedStrong() {
  return (tree: MarkdownRoot) => {
    function repair(node: { type: string; children?: unknown[] }) {
      // CommonMark leaves a closing ** literal when a curly quote is
      // immediately followed by a word character, as with Korean particles.
      if (node.type === 'link' || node.type === 'linkReference') return;
      const children = node.children;
      if (!children) return;
      for (let index = 0; index < children.length; index++) {
        const child = children[index] as Text;
        if (child.type !== 'text' || !child.position) {
          repair(child);
          continue;
        }
        const start = child.position.start.offset;
        if (start === undefined || child.position.end.offset === undefined
          || child.position.end.offset - start !== child.value.length) continue;
        const expression = /\*\*((?:‘[^’\n]+’|“[^”\n]+”))\*\*(?=[\p{Script=Hangul}])/gu;
        const matches = [...child.value.matchAll(expression)];
        if (!matches.length) continue;
        const output: PhrasingContent[] = [];
        let offset = 0;
        for (const match of matches) {
          if (match.index > offset) output.push({ type: 'text', value: child.value.slice(offset, match.index) });
          const from = match.index + 2;
          const to = from + match[1].length;
          output.push({ type: 'strong', children: [{ type: 'text', value: match[1] }] });
          offset = to + 2;
        }
        if (offset < child.value.length) output.push({ type: 'text', value: child.value.slice(offset) });
        children.splice(index, 1, ...output);
        index += output.length - 1;
      }
    }
    repair(tree);
  };
}

// Split only changed text after parsing. The Markdown structure and line wrapping stay in place.
function rehypeAnswerTyping({ start, revision }: { start: number; revision: number }) {
  return (tree: Root) => {
    let step = 0;
    function mark(parent: Root | Element) {
      for (let index = 0; index < parent.children.length; index++) {
        const node = parent.children[index];
        if (node.type === 'element') {
          if (node.tagName !== 'pre') mark(node);
          continue;
        }
        if (node.type !== 'text' || node.position?.end.offset === undefined || node.position.end.offset <= start) continue;
        const prefixLength = Math.max(0, start - (node.position.start.offset ?? start));
        const original = node.value;
        const parts = step < 64 ? graphemes(original) : [];
        const children: Element['children'] = [];
        let offset = 0;
        let indexInText = 0;
        if (step >= 64) offset = Math.min(original.length, prefixLength);
        while (indexInText < parts.length && offset < prefixLength) offset += parts[indexInText++].length;
        if (offset) children.push({ type: 'text', value: original.slice(0, offset) });
        while (indexInText < parts.length && step < 64) {
          const part = parts[indexInText++];
          children.push({
            type: 'element', tagName: 'span',
            properties: { dataTypingStep: step++, dataTypingRevision: revision },
            children: [{ type: 'text', value: part }],
          });
          offset += part.length;
        }
        if (offset < original.length) children.push({
          type: 'element', tagName: 'span',
          properties: { dataTypingStep: step, dataTypingRevision: revision },
          children: [{ type: 'text', value: original.slice(offset) }],
        });
        if (children.length === 0) continue;
        parent.children.splice(index, 1, ...children);
        index += children.length - 1;
      }
    }
    mark(tree);
  };
}

export const MarkdownAnswer = memo(function MarkdownAnswer({ source, animateUpdates = false, animateArrival = false }: { source: string; animateUpdates?: boolean; animateArrival?: boolean }) {
  const previous = useRef({ source: animateArrival ? '' : source, revision: 0 });
  const changed = source !== previous.current.source;
  const revision = previous.current.revision + Number(changed);
  let start = 0;
  if (changed) {
    while (start < source.length && source[start] === previous.current.source[start]) start++;
  }
  const typing = animateUpdates && changed && start < source.length && !document.hidden
    && !window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  useLayoutEffect(() => { previous.current = { source, revision }; }, [source, revision]);

  return <div className="answer-markdown">
    <Markdown skipHtml remarkPlugins={[remarkGfm, remarkQuotedStrong]} rehypePlugins={typing ? [[rehypeAnswerTyping, { start, revision }]] : undefined}
      components={components}>{source}</Markdown>
  </div>;
});
