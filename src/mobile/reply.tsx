import { useEffect, useId, useLayoutEffect, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { ArrowDown, ArrowLeft, Image, LoaderCircle, Paperclip, Plus, X } from 'lucide-react';
import { requestReplyContext, type Device, type Receipt, type ReplyCode, type ReplyContext } from './connection';
import type { Session } from '../types';
import { ProviderLogo } from '../provider-logo';
import { MarkdownAnswer } from './markdown-answer';
import { attachmentBytes, attachmentTypes, bytesPerMB, maxAttachments, prepareAttachment, type ReplyAttachment } from './reply-attachment';
import { matchOutgoing, sameTurns, turnKey, type ConversationCache, type ConversationPosition, type OutgoingMessage } from './conversation-state';

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
export type ReplyProgress = {
  id: string; code: ReplyCode; deadline: number; startedAt?: number; confirmedAt?: number;
  retryable?: boolean; message?: OutgoingMessage; choiceToken?: string;
};

export function ReplyComposer({ device, target, unavailable, permit, canLoadAnswer, canAttach, maxTotalBytes, textOnly, initialConversation, receipt, receipts, context, draft, progress, uploadPercent, otherPending, onDraft, onSend, onChoose, onRetry, onDismiss, onWait, onConversation, onClose }:
  { device: Device; target: Session; unavailable?: ReactNode; permit?: string | null; canLoadAnswer: boolean; canAttach: boolean; maxTotalBytes: number; textOnly?: boolean; initialConversation?: ConversationCache; receipt?: Receipt | null; receipts?: Receipt[]; context?: ReplyContext; draft: ReplyDraft; progress?: ReplyProgress; uploadPercent?: number; otherPending: boolean; onDraft(draft: ReplyDraft): void; onSend(): void; onChoose?: (answers: number[]) => void; onRetry?: () => void; onDismiss(id: string): void; onWait(waiting: boolean): void; onConversation(key: string, conversation: ConversationCache): void; onClose(): void }) {
  const selection = useRef(0);
  const dialog = useRef<HTMLDialogElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const conversation = useRef<HTMLElement>(null);
  const form = useRef<HTMLFormElement>(null);
  const position = useRef(initialConversation?.position);
  const previousTail = useRef<string | undefined>(undefined);
  const animatedTurn = useRef<string | undefined>(undefined);
  const input = useRef<HTMLTextAreaElement>(null);
  const picker = useRef<HTMLInputElement>(null);
  const [preparing, setPreparing] = useState(false);
  const [attachmentError, setAttachmentError] = useState('');
  const titleId = useId();
  const [contextId, setContextId] = useState('');
  const [contextRevision, refreshContext] = useState(0);
  const [cachedTurns, setCachedTurns] = useState(initialConversation?.turns);
  const contextSeen = useRef('');
  const [contextFailed, setContextFailed] = useState(false);
  const [receivedAnswer, setReceivedAnswer] = useState(initialConversation?.answer);
  const [newReply, setNewReply] = useState(false);
  const [unsyncedId, setUnsyncedId] = useState<string>();
  const { text, attachments = [] } = draft;
  const maxImageBytes = Math.min(5 * bytesPerMB, maxTotalBytes);
  const totalBytes = attachments.reduce((sum, file) => sum + attachmentBytes(file), 0);
  const sizeError = `파일 전체 크기를 ${maxTotalBytes / bytesPerMB}MB 이하로 줄여 주세요.`;
  const attachmentMessage = totalBytes > maxTotalBytes ? sizeError : attachmentError;
  const outcome = progress?.code;
  const pending = outcome === 'processing';
  const locked = pending || outcome === 'unknown';
  const choiceProgress = Boolean(progress?.choiceToken);
  const sentChoice = choiceProgress && outcome === 'sent';
  const answer = context?.id === contextId ? context : undefined;
  const incomingTurns = answer?.turns;
  const turns = incomingTurns && !sameTurns(cachedTurns, incomingTurns) ? incomingTurns : cachedTurns;
  const preview = answer?.answer ?? receivedAnswer ?? target.preview;
  const receivedPreview = answer?.answer ?? receivedAnswer;
  const previewRole = receivedPreview ? 'assistant' : target.role;
  const failed = answer?.failed ?? (contextFailed || receipt?.id === contextId || receipts?.some(result => result.id === contextId));
  const loadingAnswer = canLoadAnswer && !answer && !failed;
  const targetId = target.id;
  const conversationKey = `${target.provider}:${targetId}`;
  const folder = target.cwd.split('/').filter(Boolean).at(-1);
  const outgoing = choiceProgress ? undefined : progress?.message ?? (locked && progress?.startedAt && text
    ? { text, after: progress.startedAt - 1 } : undefined);
  const matched = turns && outgoing ? matchOutgoing(turns, outgoing) : -1;
  const accepted = !choiceProgress && (outcome === 'queued' || outcome === 'sent');
  const needsSync = accepted && matched < 0;
  const showOutgoing = Boolean(outgoing && matched < 0 && (locked || accepted));
  const awaitingAnswer = Boolean(matched >= 0 && turns?.at(-1)?.role === 'user' && (accepted || pending));
  const responding = target.active === true && !target.waiting && (awaitingAnswer || (!pending && !needsSync));
  const displayText = locked && !choiceProgress ? '' : text;
  const displayAttachments = locked && !choiceProgress ? [] : attachments;
  const latest = turns?.at(-1);
  const tail = latest && turnKey(latest) + ':' + latest.text.length;

  function rememberPosition() {
    const view = conversation.current;
    if (!view) return;
    const bottom = view.scrollHeight - view.clientHeight - view.scrollTop <= 30;
    if (bottom) setNewReply(false);
    const inset = view.getBoundingClientRect().top;
    const anchor = [...view.querySelectorAll<HTMLElement>('[data-turn-key]')]
      .find(element => element.getBoundingClientRect().bottom > inset);
    position.current = { bottom, key: anchor?.dataset.turnKey, offset: anchor ? anchor.getBoundingClientRect().top - inset : 0 };
    onConversation(conversationKey, { position: position.current });
  }

  function followBottom() {
    const view = conversation.current;
    if (view) view.scrollTop = view.scrollHeight;
  }

  useLayoutEffect(() => {
    const element = dialog.current;
    if (!element) return;
    const scroll = window.scrollY;
    const root = document.documentElement;
    root.style.setProperty('--offtab-page-top', `${-scroll}px`);
    if (heading.current) heading.current.autofocus = true;
    element.showModal();
    const viewport = window.visualViewport;
    const fitViewport = () => {
      if (viewport && viewport.scale === 1) {
        element.style.setProperty('--reply-height', `${viewport.height}px`);
        element.style.setProperty('--reply-top', `${viewport.offsetTop}px`);
        element.toggleAttribute('data-keyboard', window.innerHeight - viewport.height > 100);
      } else {
        element.style.removeProperty('--reply-height');
        element.style.removeProperty('--reply-top');
      }
      if (position.current?.bottom) followBottom();
    };
    fitViewport();
    viewport?.addEventListener('resize', fitViewport);
    viewport?.addEventListener('scroll', fitViewport);
    if (!viewport) window.addEventListener('resize', fitViewport);
    const resize = new ResizeObserver(() => { if (position.current?.bottom) followBottom(); });
    if (form.current) resize.observe(form.current);
    return () => {
      viewport?.removeEventListener('resize', fitViewport);
      viewport?.removeEventListener('scroll', fitViewport);
      if (!viewport) window.removeEventListener('resize', fitViewport);
      resize.disconnect();
      selection.current++;
      element.close();
      root.style.removeProperty('--offtab-page-top');
      if (window.scrollY !== scroll) window.scrollTo(0, scroll);
    };
  }, []);

  useLayoutEffect(() => { fitInput(input.current); }, [displayText, permit]);

  useLayoutEffect(() => {
    const view = conversation.current;
    if (!view) return;
    const priorTail = previousTail.current;
    previousTail.current = tail;
    const saved = position.current;
    const bubbles = [...view.querySelectorAll<HTMLElement>('[data-turn-key]')];
    if (saved?.bottom) followBottom();
    else if (saved?.key) {
      const anchor = bubbles.find(element => element.dataset.turnKey === saved.key);
      if (anchor) view.scrollTop += anchor.getBoundingClientRect().top - view.getBoundingClientRect().top - saved.offset;
      if (priorTail && tail && tail !== priorTail && latest?.role === 'assistant') setNewReply(true);
    } else if (bubbles.length) {
      const last = bubbles.at(-1)!;
      view.scrollTop = Math.min(view.scrollHeight - view.clientHeight,
        view.scrollTop + last.getBoundingClientRect().top - view.getBoundingClientRect().top - 10);
    }
    if (bubbles.length) rememberPosition();
  }, [turns, tail, showOutgoing, responding]);

  useLayoutEffect(() => {
    if (pending) {
      followBottom();
      rememberPosition();
    }
  }, [pending]);

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
    if (answer?.id && !answer.failed) {
      contextSeen.current = answer.id;
      if (turns !== cachedTurns) setCachedTurns(turns);
    } else if (canLoadAnswer && !context && contextId && contextSeen.current === contextId) {
      contextSeen.current = '';
      refreshContext(value => value + 1);
    }
  }, [answer, context, contextId, canLoadAnswer, turns, cachedTurns]);

  useLayoutEffect(() => {
    if (!answer || answer.failed) return;
    const body = answer.answer ?? turns?.findLast(turn => turn.role === 'assistant')?.text;
    if (body) setReceivedAnswer(body);
    onConversation(conversationKey, { turns, answer: body });
  }, [answer, turns, conversationKey, onConversation]);

  useEffect(() => {
    if (accepted && matched >= 0 && progress) onDismiss(progress.id);
  }, [accepted, matched, progress, onDismiss]);

  useEffect(() => {
    if (!needsSync || !progress) return;
    const elapsed = Date.now() - (progress.confirmedAt ?? 0);
    const timer = setTimeout(() => setUnsyncedId(progress.id), Math.max(0, 30_000 - elapsed));
    return () => clearTimeout(timer);
  }, [needsSync, progress]);

  useEffect(() => {
    onWait(loadingAnswer || needsSync || responding);
    return () => onWait(false);
  }, [onWait, loadingAnswer, needsSync, responding]);

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
  const sendDisabled = locked || needsSync || otherPending || preparing || Boolean(unavailable)
    || (!text.trim() && !attachments.length) || (attachments.length > 0 && !canAttach) || totalBytes > maxTotalBytes || invalidPrompt;

  function send(event: FormEvent) {
    event.preventDefault();
    if (!sendDisabled && permit) {
      input.current?.blur();
      onSend();
    }
  }

  return <dialog ref={dialog} className="reply-dialog" aria-labelledby={titleId} onCancel={event => {
    event.preventDefault();
    onClose();
  }}>
    <header className="reply-header">
      <button type="button" aria-label="세션 목록으로 돌아가기" onClick={onClose}>
        <ArrowLeft aria-hidden="true" />
      </button>
      <ProviderLogo provider={target.provider} />
      <div className="reply-title">
        <h2 ref={heading} id={titleId} tabIndex={-1}>{target.title || '제목 없는 세션'}</h2>
        {folder && <p>{folder}</p>}
      </div>
    </header>
    <section ref={conversation} className="reply-answer" aria-label="최근 대화" tabIndex={0} onScroll={rememberPosition}>
      <ol className="conversation-turns" role="list">
        {loadingAnswer && !turns?.length && <li className="conversation-fetch" role="status">대화 불러오는 중…</li>}
        {turns?.length ? turns.map((turn, index) => {
          const key = turn.id ?? turnKey(turn) + ':' + turns.slice(0, index).filter(previous => turnKey(previous) === turnKey(turn)).length;
          const fresh = Boolean(tail && (animatedTurn.current === key || (previousTail.current && previousTail.current !== tail)));
          if (turn === latest && turn.role === 'assistant' && fresh) animatedTurn.current = key;
          return <li key={key} data-turn-key={key} data-role={turn.role}>
            <span className="sr-only">{turn.role === 'assistant' ? 'AI 답변' : '내 메시지'}</span>
            {turn.role === 'assistant' ? <MarkdownAnswer source={turn.text} animateUpdates={turn === latest} animateArrival={fresh && turn === latest} />
              : <ReplyBubble text={turn.text} attachments={turn.attachments ?? (index === matched ? outgoing?.attachments : undefined)} />}
            {index === matched && locked && progress && <small className="message-delivery" role="status">{replyLabels[progress.code]}</small>}
            {turn.truncated && <small>긴 메시지의 일부를 표시하고 있습니다.</small>}
          </li>;
        }) : preview && <li data-role={previewRole}>
          {previewRole === 'user' ? <ReplyBubble text={preview} /> : <MarkdownAnswer source={preview} />}
        </li>}
        {showOutgoing && outgoing && progress && <li data-turn-key={progress.id} data-role="user">
          <ReplyBubble text={outgoing.text} attachments={outgoing.attachments} />
          <small className="message-delivery" data-error={outcome === 'unknown'} role="status">
            {pending && <LoaderCircle aria-hidden="true" />}
            {pending ? '전송 중' : progress.code === 'queued' ? '대기열에 있음' : progress.code === 'sent' ? '전송됨' : '전송 여부 확인 필요'}
            {pending && uploadPercent !== undefined && uploadPercent < 100 && <> · 첨부 {uploadPercent}%</>}
          </small>
          {needsSync && unsyncedId === progress.id && <button className="message-status-dismiss" type="button"
            onClick={() => onDismiss(progress.id)}>전송 상태 닫기</button>}
        </li>}
        {responding && <li className="conversation-responding" role="status">
          <LoaderCircle aria-hidden="true" /> 응답 중
        </li>}
        {target.waiting && <WaitingCard key={target.choiceToken ?? target.waitingAt} target={target} onChoose={onChoose}
          disabled={locked || otherPending || (sentChoice && progress?.choiceToken === target.choiceToken)} />}
      </ol>
      {!loadingAnswer && failed && <div className="conversation-actions">
        <p role="status">최근 답변을 불러오지 못했습니다.</p>
        <button type="button" disabled={!canLoadAnswer} onClick={() => refreshContext(value => value + 1)}>다시 불러오기</button>
      </div>}
      {!loadingAnswer && !failed && !preview && !turns?.length && !showOutgoing && !responding && !target.waiting && <p>표시할 메시지가 없습니다.</p>}
    </section>
    {newReply && <button className="conversation-latest" type="button" onClick={() => {
      followBottom();
      setNewReply(false);
    }}><ArrowDown aria-hidden="true" /> 새 답변</button>}
    <form ref={form} className="reply-form" onSubmit={send}>
      {permit && <>
        {displayAttachments.map((attachment, index) => <div className="reply-attachment" key={attachment.name + ':' + index}>
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
          <textarea ref={input} rows={1} maxLength={2_000} value={displayText} readOnly={locked || preparing} aria-label="답장"
            onChange={event => onDraft({ ...draft, text: event.currentTarget.value })} placeholder="답장을 입력하세요" />
          <button type="submit" disabled={sendDisabled}>보내기</button>
        </div>
        {attachmentMessage ? <p role="status">{attachmentMessage}</p> : preparing && <p role="status">첨부 파일 준비 중…</p>}
      </>}
      {unavailable && !showOutgoing && !responding && <p role="status">{unavailable}</p>}
      {otherPending && <p role="status">다른 채팅의 답장 전송을 확인하는 중입니다.</p>}
      {choiceProgress && pending && <p role="status">원본 채팅에 선택지를 전송하는 중이에요.</p>}
      {sentChoice && progress && <p role="status">원본 채팅에 선택지를 보냈어요. <button type="button" className="choice-receipt-dismiss" onClick={() => onDismiss(progress.id)}>닫기</button></p>}
      {outcome && outcome !== 'processing' && !accepted && !sentChoice && <p className="reply-error" role="status">{replyLabels[outcome]}</p>}
      {onRetry && <button type="button" onClick={onRetry}>같은 요청 다시 보내기</button>}
      {outcome === 'unknown' && progress && <button type="button" onClick={() => onDismiss(progress.id)}>전송 확인 안내 닫기</button>}
      {invalidPrompt && <p role="status">일반 텍스트 메시지를 입력하세요.</p>}
    </form>
  </dialog>;
}

function WaitingCard({ target, disabled, onChoose }: { target: Session; disabled: boolean; onChoose?: (answers: number[]) => void }) {
  const [selected, setSelected] = useState<Record<number, number>>({});
  const questions = target.waitingDetails?.questions ?? [];
  const single = questions.length === 1;
  return <li className="conversation-waiting">
    <p className="conversation-waiting-label">{target.waiting === 'approval' ? '승인 대기' : '선택·답변 대기'}</p>
    {target.waitingDetails?.tool && <p>요청 도구 <code>{target.waitingDetails.tool}</code></p>}
    {questions.map(({ question, options }, index) => <div key={index}>
      <p>{question}</p>
      {options.length > 0 && <ul aria-label="선택지" className={onChoose ? 'conversation-choice-list' : undefined}>
        {options.map((option, position) => <li key={position}>{onChoose
          ? <button type="button" disabled={disabled} aria-pressed={single ? undefined : selected[index] === position}
              onClick={() => single ? onChoose([position]) : setSelected(previous => ({ ...previous, [index]: position }))}>{option}</button>
          : option}</li>)}
      </ul>}
    </div>)}
    {onChoose && !single && questions.length > 1 && <button type="button" className="conversation-choice-submit"
      disabled={disabled || questions.some((_, index) => selected[index] === undefined)}
      onClick={() => onChoose(questions.map((_, index) => selected[index]))}>선택지 보내기</button>}
    {!target.waitingDetails && <p className="conversation-waiting-note">자세한 내용은 Mac의 원본 채팅에서 볼 수 있어요.</p>}
  </li>;
}

function ReplyBubble({ text, attachments }: { text: string; attachments?: OutgoingMessage['attachments'] }) {
  return <div className="reply-bubble">
    {text && <p className="reply-content">{text}</p>}
    {attachments?.length ? <ul className="message-attachments" aria-label="첨부">
      {attachments.map(({ kind, name }, index) => <li key={index}>
        {kind === 'image' ? <Image aria-hidden="true" /> : <Paperclip aria-hidden="true" />}
        <span>{name || (kind === 'image' ? '첨부 사진' : '첨부 파일')}</span>
      </li>)}
    </ul> : null}
  </div>;
}
