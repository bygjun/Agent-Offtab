import { useEffect, useId, useLayoutEffect, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { ArrowLeft, Paperclip, Plus, X } from 'lucide-react';
import { requestReplyContext, type Device, type Receipt, type ReplyCode, type ReplyContext } from './connection';
import type { Session } from '../types';
import { MarkdownAnswer } from './markdown-answer';
import { attachmentBytes, attachmentTypes, bytesPerMB, maxAttachments, prepareAttachment, type ReplyAttachment } from './reply-attachment';

export const replyLabels: Record<ReplyCode, string> = {
  processing: 'Mac이 채팅에 전달하는 중입니다.',
  queued: 'Codex 대기열에 추가했어요.',
  sent: '기존 채팅에 답장을 보냈습니다.',
  blocked: '답장을 보낼 수 없습니다. Mac의 기기 권한과 메시지를 확인하세요.',
  'invalid-attachment': '첨부 파일을 확인해 주세요. 다른 파일을 선택해 다시 보내세요.',
  busy: '세션 상태가 바뀌었거나 작업 중입니다. 최근 내용을 확인해 주세요.',
  unavailable: 'Mac에서 이 세션에 답장을 보낼 수 없습니다.',
  'desktop-permission': 'Mac Offtab의 휴대폰 연결에서 손쉬운 사용 설정을 열고 Offtab을 허용해 주세요.',
  'desktop-locked': 'Mac의 잠금을 해제하고 다시 보내 주세요.',
  'desktop-draft': 'Claude 채팅에 작성 중인 메시지가 있어요. Mac에서 입력창을 확인한 후 다시 보내 주세요.',
  'desktop-target': 'Claude 입력창을 확인할 수 없어요. Mac에서 이 채팅을 열고 입력창이 보이는 상태로 다시 보내 주세요.',
  unknown: '전송 여부를 확인하지 못했습니다. 기존 채팅에 답장이 있는지 확인하세요.',
};

function fitInput(input: HTMLTextAreaElement | null) {
  if (!input) return;
  const scroll = input.scrollTop;
  input.style.height = '0px';
  input.style.height = `${input.scrollHeight + input.offsetHeight - input.clientHeight}px`;
  input.scrollTop = scroll;
}

export type ReplyDraft = { text: string; attachments?: ReplyAttachment[] };
export type ReplyProgress = { id: string; code: ReplyCode; deadline: number; startedAt?: number; retryable?: boolean };

export function ReplyComposer({ device, target, unavailable, permit, canLoadAnswer, canAttach, maxTotalBytes, textOnly, initialAnswer, receipt, receipts, context, draft, outcome, uploadPercent, otherPending, onDraft, onSend, onRetry, onDismiss, onWait, onAnswer, onClose }:
  { device: Device; target: Session; unavailable?: ReactNode; permit?: string | null; canLoadAnswer: boolean; canAttach: boolean; maxTotalBytes: number; textOnly?: boolean; initialAnswer?: string; receipt?: Receipt | null; receipts?: Receipt[]; context?: ReplyContext; draft: ReplyDraft; outcome?: ReplyCode; uploadPercent?: number; otherPending: boolean; onDraft(draft: ReplyDraft): void; onSend(): void; onRetry?: () => void; onDismiss(): void; onWait(waiting: boolean): void; onAnswer(session: Session, body: string): void; onClose(): void }) {
  const selection = useRef(0);
  const dialog = useRef<HTMLDialogElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const conversation = useRef<HTMLElement>(null);
  const latestMessage = useRef<HTMLLIElement>(null);
  const positioned = useRef(false);
  const input = useRef<HTMLTextAreaElement>(null);
  const picker = useRef<HTMLInputElement>(null);
  const [preparing, setPreparing] = useState(false);
  const [attachmentError, setAttachmentError] = useState('');
  const titleId = useId();
  const [contextId, setContextId] = useState('');
  const [contextRevision, refreshContext] = useState(0);
  const [cachedTurns, setCachedTurns] = useState<ReplyContext['turns']>();
  const contextSeen = useRef('');
  const [contextFailed, setContextFailed] = useState(false);
  const [receivedAnswer, setReceivedAnswer] = useState(initialAnswer);
  const { text, attachments = [] } = draft;
  const maxImageBytes = Math.min(5 * bytesPerMB, maxTotalBytes);
  const totalBytes = attachments.reduce((sum, file) => sum + attachmentBytes(file), 0);
  const sizeError = `파일 전체 크기를 ${maxTotalBytes / bytesPerMB}MB 이하로 줄여 주세요.`;
  const attachmentMessage = totalBytes > maxTotalBytes ? sizeError : attachmentError;
  const pending = outcome === 'processing';
  const locked = pending || outcome === 'unknown';
  const answer = context?.id === contextId ? context : undefined;
  const turns = answer?.turns ?? cachedTurns;
  const markdown = answer?.answer ?? receivedAnswer;
  const failed = answer?.failed ?? (contextFailed || receipt?.id === contextId || receipts?.some(result => result.id === contextId));
  const loadingAnswer = canLoadAnswer && !answer && !failed;
  const targetId = target.id;
  const folder = target.cwd.split('/').filter(Boolean).at(-1);
  const answerHeading = markdown || !target.preview ? '최근 AI 답변'
    : target.role === 'user' ? '최근 사용자 메시지'
      : target.role === 'assistant' ? '최근 AI 답변 미리보기' : '최근 메시지';

  useLayoutEffect(() => {
    const element = dialog.current;
    if (!element) return;
    if (heading.current) heading.current.autofocus = true;
    element.showModal();
    const viewport = window.visualViewport;
    const fitViewport = () => {
      if (viewport && viewport.scale === 1) {
        element.style.setProperty('--reply-height', `${viewport.height}px`);
        element.style.setProperty('--reply-top', `${viewport.offsetTop}px`);
      } else {
        element.style.removeProperty('--reply-height');
        element.style.removeProperty('--reply-top');
      }
      fitInput(input.current);
    };
    fitViewport();
    viewport?.addEventListener('resize', fitViewport);
    viewport?.addEventListener('scroll', fitViewport);
    if (!viewport) window.addEventListener('resize', fitViewport);
    return () => {
      viewport?.removeEventListener('resize', fitViewport);
      viewport?.removeEventListener('scroll', fitViewport);
      if (!viewport) window.removeEventListener('resize', fitViewport);
      selection.current++;
      element.close();
    };
  }, []);

  useLayoutEffect(() => { fitInput(input.current); }, [text, permit]);

  useLayoutEffect(() => {
    const view = conversation.current;
    const latest = latestMessage.current;
    if (!view || !latest || positioned.current) return;
    positioned.current = true;
    if (view.scrollTop > 0) return;
    // Keep the beginning of a long latest reply visible and let earlier turns scroll above it.
    const inset = parseFloat(getComputedStyle(view).paddingTop);
    view.scrollTop = latest.getBoundingClientRect().top - view.getBoundingClientRect().top - inset;
  }, [turns]);

  useEffect(() => {
    if (!canLoadAnswer) return;
    const id = crypto.randomUUID();
    setContextId(id);
    setContextFailed(false);
    let mounted = true;
    let retry: ReturnType<typeof setTimeout>;
    const timeout = setTimeout(() => {
      mounted = false;
      clearTimeout(retry);
      setContextFailed(true);
    }, 20_000);
    const request = (remaining: number) => {
      void requestReplyContext(device, id, targetId, 10, permit).then(code => {
        if (!mounted) return;
        if (code === 'busy' && remaining > 0) {
          retry = setTimeout(() => request(remaining - 1), 1_100);
        } else if (code) setContextFailed(true);
      }, () => { if (mounted) setContextFailed(true); });
    };
    request(12);
    return () => { mounted = false; clearTimeout(timeout); clearTimeout(retry); };
  }, [device, permit, canLoadAnswer, targetId, contextRevision]);

  useEffect(() => {
    if (answer?.id) {
      contextSeen.current = answer.id;
      if (answer.turns) setCachedTurns(answer.turns);
    } else if (canLoadAnswer && !context && contextId && contextSeen.current === contextId) {
      contextSeen.current = '';
      refreshContext(value => value + 1);
    }
  }, [answer, context, contextId, canLoadAnswer]);

  const body = answer?.answer ?? answer?.turns?.findLast(turn => turn.role === 'assistant')?.text;
  useEffect(() => {
    if (body) {
      setReceivedAnswer(body);
      onAnswer(target, body);
    }
  }, [body, target, onAnswer]);

  useEffect(() => {
    onWait(loadingAnswer);
    return () => onWait(false);
  }, [onWait, loadingAnswer]);

  async function selectAttachments(files: File[]) {
    const operation = ++selection.current;
    setPreparing(true);
    setAttachmentError('');
    try {
      if (attachments.length + files.length > maxAttachments) throw new Error('파일은 한 번에 4개까지 첨부할 수 있습니다.');
      const added = await Promise.all(files.map(file => prepareAttachment(file, maxImageBytes)));
      if (added.reduce((sum, file) => sum + attachmentBytes(file), totalBytes) > maxTotalBytes) throw new Error(sizeError);
      const next = [...attachments, ...added];
      if (operation === selection.current) onDraft({ ...draft, attachments: next });
    } catch (failure) {
      if (operation === selection.current) {
        setAttachmentError(failure instanceof Error ? failure.message : '파일을 열 수 없습니다.');
      }
    } finally {
      if (operation === selection.current) setPreparing(false);
    }
  }

  const invalidPrompt = /^[!\/]/.test(text.trim());
  const sendDisabled = locked || otherPending || preparing || Boolean(unavailable)
    || (!text.trim() && !attachments.length) || (attachments.length > 0 && !canAttach) || totalBytes > maxTotalBytes || invalidPrompt;

  function send(event: FormEvent) {
    event.preventDefault();
    if (!sendDisabled && permit) onSend();
  }

  return <dialog ref={dialog} className="reply-dialog" aria-labelledby={titleId} onCancel={event => {
    event.preventDefault();
    onClose();
  }}>
    <header className="reply-header">
      <button type="button" aria-label="세션 목록으로 돌아가기" onClick={onClose}>
        <ArrowLeft aria-hidden="true" />
      </button>
      <div className="reply-title">
        <h2 ref={heading} id={titleId} tabIndex={-1}>{target.title || '제목 없는 세션'}</h2>
        {folder && <p>{folder}</p>}
      </div>
    </header>
    <section ref={conversation} className="reply-answer" aria-label="최근 대화" tabIndex={0}>
      {turns?.length ? <ol className="conversation-turns" role="list">
        {turns.map((turn, index) => <li ref={index === turns.length - 1 ? latestMessage : undefined} key={turn.role + ':' + index} data-role={turn.role}>
          <span className="sr-only">{turn.role === 'assistant' ? 'AI 답변' : '내 메시지'}</span>
          {turn.role === 'assistant' ? <MarkdownAnswer source={turn.text} animateUpdates={index === turns.length - 1} />
            : <p className="reply-content reply-bubble">{turn.text}</p>}
          {turn.truncated && <small>긴 메시지의 일부를 표시하고 있습니다.</small>}
        </li>)}
      </ol> : <>
        <h3>{answerHeading}</h3>
        {markdown ? <MarkdownAnswer source={markdown} animateUpdates /> : target.preview && (target.role === 'assistant'
          ? <MarkdownAnswer source={target.preview} /> : <p className={target.role === 'user' ? 'reply-content reply-bubble' : 'reply-content'}>{target.preview}</p>)}
      </>}
      {loadingAnswer && <p role="status">답변을 불러오는 중입니다.</p>}
      {!loadingAnswer && failed && <div className="conversation-actions">
        <p role="status">최근 답변을 불러오지 못했습니다.</p>
        <button type="button" disabled={!canLoadAnswer} onClick={() => refreshContext(value => value + 1)}>다시 불러오기</button>
      </div>}
      {!loadingAnswer && !failed && !target.preview && !markdown && !turns?.length && <p>표시할 메시지가 없습니다.</p>}
    </section>
    <form className="reply-form" onSubmit={send}>
      {permit && <>
        {attachments.map((attachment, index) => <div className="reply-attachment" key={attachment.name + ':' + index}>
          <Paperclip aria-hidden="true" />
          <span title={attachment.name}>{attachment.name} · {Math.ceil(attachmentBytes(attachment) / 1024)}KB</span>
          <button type="button" disabled={locked || preparing} aria-label={attachment.name + ' 제거'} onClick={() => {
            onDraft({ text, attachments: attachments.filter((_, position) => position !== index) });
            setAttachmentError('');
          }}><X aria-hidden="true" /></button>
        </div>)}
        <div className={textOnly ? 'reply-compose' : 'reply-compose with-attachment'}>
          {!textOnly && <>
            <input ref={picker} type="file" accept={attachmentTypes} multiple hidden onChange={event => {
              const files = Array.from(event.currentTarget.files ?? []);
              event.currentTarget.value = '';
              if (files.length) void selectAttachments(files);
            }} />
            <button type="button" className="reply-attach-button" aria-label="사진 또는 파일 첨부"
              title={canAttach ? '사진 또는 파일 첨부' : '이 세션은 지금 첨부를 받을 수 없습니다.'}
              disabled={!canAttach || locked || preparing || Boolean(unavailable)}
              onClick={() => picker.current?.click()}><Plus aria-hidden="true" /></button>
          </>}
          <textarea ref={input} rows={1} maxLength={2_000} value={text} readOnly={locked || preparing} aria-label="답장"
            onChange={event => onDraft({ ...draft, text: event.currentTarget.value })} placeholder="답장을 입력하세요" />
          <button type="submit" disabled={sendDisabled}>보내기</button>
        </div>
        {attachmentMessage ? <p role="status">{attachmentMessage}</p> : preparing && <p role="status">첨부 파일 준비 중…</p>}
      </>}
      {unavailable && <p role="status">{unavailable}</p>}
      {otherPending && <p role="status">다른 채팅의 답장 전송을 확인하는 중입니다.</p>}
      {uploadPercent !== undefined && uploadPercent < 100 && outcome === 'processing'
        ? <label className="reply-upload">첨부 업로드 {uploadPercent}% <progress value={uploadPercent} max={100} /></label>
        : outcome && <p role="status">{replyLabels[outcome]}</p>}
      {onRetry && <button type="button" onClick={onRetry}>같은 요청 다시 보내기</button>}
      {outcome === 'unknown' && <button type="button" onClick={onDismiss}>전송 확인 안내 닫기</button>}
      {invalidPrompt && <p role="status">일반 텍스트 메시지를 입력하세요.</p>}
    </form>
  </dialog>;
}
