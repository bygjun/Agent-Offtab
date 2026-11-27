import { useLayoutEffect, useRef, useState } from 'react';

const segmenter = typeof Intl.Segmenter === 'function' ? new Intl.Segmenter() : undefined;

export function graphemes(text: string) {
  return segmenter ? Array.from(segmenter.segment(text), part => part.segment) : Array.from(text);
}

export function TypedPreview({ text }: { text: string }) {
  const latest = useRef(text);
  const preview = useRef<HTMLSpanElement>(null);
  const [frame, setFrame] = useState<{ text: string; length: number }>();

  useLayoutEffect(() => {
    const previous = latest.current;
    latest.current = text;
    if (previous === text || !text) return;
    const reduced = window.matchMedia('(prefers-reduced-motion: reduce)');
    const rendered = preview.current?.getBoundingClientRect();
    const viewport = preview.current?.closest('main')?.getBoundingClientRect();
    if (document.hidden || reduced.matches || !rendered || !viewport
      || rendered.top >= viewport.bottom || rendered.bottom <= viewport.top) return;

    const start = previous && text.startsWith(previous) ? previous.length : 0;
    const suffix = text.slice(start);
    let end = start;
    const positions = [start];
    for (const part of graphemes(suffix)) {
      end += part.length;
      positions.push(end);
    }
    const ticks = Math.min(18, positions.length - 1);
    let step = 0;
    const interval = window.setInterval(() => {
      step++;
      if (document.hidden || reduced.matches || step >= ticks) {
        setFrame(undefined);
        window.clearInterval(interval);
        return;
      }
      setFrame({ text, length: positions[Math.floor(step * (positions.length - 1) / ticks)] });
    }, 38);
    setFrame({ text, length: start });
    return () => window.clearInterval(interval);
  }, [text]);

  const typing = frame?.text === text;
  if (!text) return null;
  return (
    <span className="typed-preview" ref={preview}>
      <span className={typing ? 'preview preview-reserve' : 'preview'}>{text}</span>
      {typing && <span className="preview preview-typing" aria-hidden="true">{text.slice(0, frame.length)}</span>}
    </span>
  );
}
