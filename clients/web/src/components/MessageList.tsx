import { useState, useEffect, useRef, useCallback, useMemo, memo, createContext, useContext, type ReactNode } from 'react'
import type { RefObject } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeHighlight from 'rehype-highlight'
import { ChevronDown, ChevronRight, Copy, Check, ArrowDown, ArrowUp, RotateCcw, Pencil, Trash2, Pin } from 'lucide-react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from './Toast'
import Lightbox from 'yet-another-react-lightbox'
import Zoom from 'yet-another-react-lightbox/plugins/zoom'
import Download from 'yet-another-react-lightbox/plugins/download'
import Fullscreen from 'yet-another-react-lightbox/plugins/fullscreen'
import 'yet-another-react-lightbox/styles.css'
import type { ChatMessage, MessageBlock } from '../hooks/useWebSocket'
import ToolCallCard from './ToolCallCard'
import { hasMath, normalizeMathDelimiters, closeUnclosedFences, ensureKatexCss, loadRehypeKatex, isRehypeKatexLoaded, getRehypeKatex } from '../lib/mathUtils'
import { searchableMessageText, messageTimestamp, timeDividerLabel, shouldShowTimeDivider, highlightText } from '../lib/searchUtils'
import {
  releaseUnusedBlobUrls,
  getImageCache,
  FileRequestContext,
  LazyImage,
  renderFileRef
} from '../lib/fileUtils'

// ── Error Boundary ────────────────────────────────────────────────────────────

function ChatErrorBoundary({ children }: { children: ReactNode }) {
  const [state, setState] = useState<{ hasError: boolean; error?: Error }>({ hasError: false })

  useEffect(() => {
    const handler = (error: Error) => {
      console.error('[ChatErrorBoundary] Uncaught error:', error)
      setState({ hasError: true, error })
    }
    window.addEventListener('error', (e) => handler(e.error))
    return () => window.removeEventListener('error', handler as any)
  }, [])

  if (state.hasError) {
    return (
      <div className="flex flex-col items-center justify-center h-full p-8 text-center">
        <div className="text-4xl mb-4">⚠️</div>
        <h2 className="text-lg font-semibold text-zinc-200 mb-2">消息渲染出错</h2>
        <p className="text-sm text-zinc-500 mb-4">
          {state.error?.message || '未知错误'}
        </p>
        <button
          onClick={() => setState({ hasError: false })}
          className="px-4 py-2 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-200 text-sm transition-colors"
        >
          重试
        </button>
      </div>
    )
  }

  return <>{children}</>
}

// ── PreCodeBlock with syntax highlighting + language label ────────────────

function PreCodeBlock({ children, className }: { children: ReactNode; className?: string }) {
  const [copied, setCopied] = useState(false)
  const preRef = useRef<HTMLPreElement>(null)
  const classes = (className || '').split(/\s+/)
  const lang = classes.find(c => c.startsWith('language-'))?.replace('language-', '') || ''

  const handleCopy = async () => {
    if (!preRef.current) return
    const text = preRef.current.innerText || ''
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch { /* ignore */ }
  }

  return (
    <div className="code-block relative group/code my-4 overflow-hidden rounded-xl border border-zinc-800 bg-zinc-950 shadow-md">
      <div className="flex items-center justify-between px-4 py-1.5 border-b border-zinc-800 bg-zinc-900/40 text-[10px] text-zinc-500 font-mono select-none">
        <span>{lang || 'code'}</span>
        <button onClick={handleCopy} className="flex items-center gap-1 px-2 py-0.5 rounded bg-zinc-900 hover:bg-zinc-800 hover:text-zinc-200 border border-zinc-800/60 transition-opacity sm:opacity-60 sm:group-hover/code:opacity-100">
          {copied ? <><Check size={10} className="text-emerald-400" /><span className="text-emerald-400">Copied</span></> : <><Copy size={10} /><span>Copy</span></>}
        </button>
      </div>
      <pre ref={preRef} className="p-2 sm:p-3 lg:p-4 overflow-x-auto text-[11px] sm:text-xs leading-5 sm:leading-6 text-zinc-400 focus:outline-none !my-0 !bg-transparent !border-none">{children}</pre>
    </div>
  )
}

// ── Generating dots ──────────────────────────────────────────────────────

function GeneratingDots() {
  return (
    <div className="space-y-2 py-1">
      <div className="skeleton-line w-full" />
      <div className="skeleton-line w-full" />
      <div className="skeleton-line w-3/5" />
    </div>
  )
}

// ── Thinking block with throttled updates ────────────────────────────────

const ThinkingBlock = memo(function ThinkingBlock({ text, isStreaming }: { text: string; isStreaming?: boolean }) {
  const [open, setOpen] = useState(false)
  const [displayText, setDisplayText] = useState(text)
  const rafRef = useRef<number | null>(null)
  const pendingTextRef = useRef(text)
  const userToggled = useRef(false)

  // Default: collapsed. Auto-open only when streaming AND user has
  // already expanded once (re-opening).  This prevents verbose reasoning
  // from flooding the UI during streaming for models like qwen3.
  const wasStreaming = useRef(false)
  useEffect(() => {
    if (isStreaming && !wasStreaming.current && userToggled.current && !open) {
      setOpen(true)
    }
    wasStreaming.current = isStreaming ?? false
  }, [isStreaming, open])

  // When streaming ends, auto-collapse if user hasn't manually opened it.
  useEffect(() => {
    if (!isStreaming && !userToggled.current) {
      setOpen(false)
    }
  }, [isStreaming])

  const handleToggle = () => {
    userToggled.current = true
    setOpen(prev => !prev)
  }

  useEffect(() => {
    pendingTextRef.current = text
    if (!rafRef.current) {
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = null
        setDisplayText(pendingTextRef.current)
      })
    }
    return () => { if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = null } }
  }, [text])

  return (
    <div className="rounded-xl border border-zinc-800 overflow-hidden text-[11px] sm:text-xs">
      <button onClick={handleToggle} className="w-full flex items-center gap-2 px-3.5 py-2.5 text-left text-zinc-500 hover:text-zinc-400 hover:bg-zinc-800/30 transition-colors">
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}<span>Thinking{isStreaming ? '…' : ''}</span>
      </button>
      {open && (
        <div className="px-3.5 py-3 text-zinc-500 leading-5 border-t border-zinc-800 bg-zinc-900/40 prose prose-invert prose-sm max-w-none prose-p:my-1 prose-li:my-0 prose-code:text-zinc-400 prose-code:bg-zinc-800/60 prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none prose-strong:text-zinc-400 prose-a:text-blue-400/70">
          <Markdown remarkPlugins={[remarkGfm]} components={{ pre: ({ children }) => <pre className="!bg-transparent !p-0 !my-1 overflow-x-auto">{children}</pre> }}>
            {displayText}
          </Markdown>
        </div>
      )}
    </div>
  )
})

// ── Content renderer ─────────────────────────────────────────────────────

type ContentSegment = { type: 'text'; text: string } | { type: 'system-reminder'; text: string }

function splitSystemReminders(text: string): ContentSegment[] {
  const segments: ContentSegment[] = []
  const re = /<system-reminder>\s*([\s\S]*?)\s*<\/system-reminder>/g
  let last = 0
  let match: RegExpExecArray | null
  let found = false

  while ((match = re.exec(text)) !== null) {
    found = true
    if (match.index > last) {
      const before = text.slice(last, match.index).trim()
      if (before) segments.push({ type: 'text', text: before })
    }
    segments.push({ type: 'system-reminder', text: match[1].trim() })
    last = match.index + match[0].length
  }

  if (!found) return [{ type: 'text', text }]

  if (last < text.length) {
    const after = text.slice(last).trim()
    if (after) segments.push({ type: 'text', text: after })
  }

  return segments
}

const SystemReminderCard = memo(function SystemReminderCard({ text }: { text: string }) {
  const [open, setOpen] = useState(false)
  const lines = text.split('\n').filter(Boolean).length
  const preview = text.split('\n').find(line => line.trim())?.trim() || 'System reminder'

  return (
    <div className="system-reminder-card not-prose rounded-xl border border-zinc-700/70 bg-zinc-900/70 overflow-hidden text-xs shadow-md relative before:absolute before:inset-y-0 before:left-0 before:w-1 before:bg-zinc-600">
      <button
        onClick={() => setOpen(prev => !prev)}
        className="w-full flex items-center gap-2 pl-4 pr-3 py-2 text-left hover:bg-zinc-800/60 transition-colors"
        title={open ? 'Collapse system reminder' : 'Expand system reminder'}
      >
        {open ? <ChevronDown size={13} className="text-zinc-500 shrink-0" /> : <ChevronRight size={13} className="text-zinc-500 shrink-0" />}
        <span className="font-mono text-zinc-300 shrink-0">system-reminder</span>
        <span className="text-zinc-500 truncate min-w-0 flex-1">{preview}</span>
        <span className="font-mono text-zinc-600 shrink-0">{lines} lines</span>
      </button>
      {open && (
        <div className="system-reminder-body pl-4 pr-3 py-2 border-t border-zinc-700/60 bg-zinc-950/30 max-h-80 overflow-y-auto">
          <div className="prose prose-invert prose-sm max-w-none prose-p:my-1 prose-li:my-0.5 prose-ul:my-1 prose-ol:my-1 prose-headings:my-2 prose-headings:text-zinc-200 prose-code:text-zinc-300 prose-code:bg-zinc-800/60 prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-code:before:content-none prose-code:after:content-none prose-pre:my-2 prose-pre:bg-zinc-950 prose-pre:border prose-pre:border-zinc-800 prose-pre:rounded-lg prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400 prose-a:text-blue-400 prose-strong:text-zinc-200">
            <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
          </div>
        </div>
      )}
    </div>
  )
})

const ContentBlock = memo(function ContentBlock({ text, done }: { text: string; done: boolean }) {
  const [katexReady, setKatexReady] = useState(isRehypeKatexLoaded())
  // Render-only pipeline: fence fix → LaTeX delimiter normalize → remark-math ($/$$)
  const prepared = useMemo(
    () => normalizeMathDelimiters(closeUnclosedFences(text)),
    [text],
  )
  const needsMath = hasMath(prepared)

  useEffect(() => {
    if (done && needsMath && !katexReady) {
      Promise.all([ensureKatexCss(), loadRehypeKatex()]).then(() => setKatexReady(true))
    }
  }, [done, needsMath, katexReady])

  const proseClasses = `prose prose-invert prose-sm lg:prose-base max-w-none prose-p:leading-6 sm:prose-p:leading-7 prose-p:my-2 first:prose-p:mt-0 prose-headings:text-zinc-100 prose-headings:font-semibold prose-headings:mt-5 prose-headings:mb-2 prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1 sm:prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded-md prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400 prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline prose-strong:text-zinc-200 prose-strong:font-semibold prose-ul:my-2 prose-ol:my-2 prose-li:my-0.5 prose-hr:border-zinc-800`
  const searchQ = useContext(SearchContext)

  if (!done) {
    // Streaming: raw text only (no math pipeline — delimiters may be incomplete)
    return <div className={`${proseClasses} whitespace-pre-wrap`}>{searchQ ? highlightText(text, searchQ) : text}</div>
  }

  const rehypePlugins: any[] = [rehypeHighlight]
  if (needsMath && katexReady && getRehypeKatex()) rehypePlugins.push(getRehypeKatex())

  return (
    <div className={proseClasses}>
      <Markdown
        remarkPlugins={needsMath && katexReady ? [remarkGfm, remarkMath] : [remarkGfm]}
        rehypePlugins={rehypePlugins}
        components={{
          table: ({ children }) => <div className="overflow-x-auto my-2"><table className="border-collapse text-xs">{children}</table></div>,
          pre: ({ children }) => {
            const codeChild = Array.isArray(children)
              ? children.find((c: any) => typeof c?.props?.className === 'string' && c.props.className.includes('language-'))
              : (typeof (children as any)?.props?.className === 'string' && (children as any).props.className.includes('language-') ? children : null)
            return <PreCodeBlock className={codeChild?.props?.className || ''}>{children}</PreCodeBlock>
          },
        }}
      >{prepared}</Markdown>
    </div>
  )
})

// ── Block renderer ───────────────────────────────────────────────────────

function renderBlock(block: MessageBlock, index: number, isGenerating: boolean) {
  if (block.type === 'content') return <ContentBlock key={index} text={block.text} done={!isGenerating} />
  if (block.type === 'thinking') return <ThinkingBlock key={index} text={block.text} isStreaming={isGenerating} />
  return <ToolCallCard key={block.id} block={block} />
}

// ── Message actions ──────────────────────────────────────────────────────

function extractText(blocks: MessageBlock[]): string {
  return blocks.filter((b): b is { type: 'content'; text: string } => b.type === 'content').map((b) => b.text).join('\n\n')
}

function MessageActions({ blocks, isLast, isGenerating, onRetry, onDelete, onPin, pinned }: { blocks: MessageBlock[]; isLast: boolean; isGenerating: boolean; onRetry?: () => void; onDelete?: () => void; onPin?: () => void; pinned?: boolean }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = async () => {
    const text = extractText(blocks)
    if (!text) return
    try { await navigator.clipboard.writeText(text); setCopied(true); setTimeout(() => setCopied(false), 2000) } catch { /* ignore */ }
  }
  return (
    <div className="flex items-center gap-0.5 mt-1">
      <button onClick={handleCopy} className="p-1.5 rounded-md text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title={copied ? 'Copied' : 'Copy message'}>
        {copied ? <Check size={14} className="text-emerald-400" /> : <Copy size={14} />}
      </button>
      {isLast && !isGenerating && onRetry && (
        <button onClick={onRetry} className="p-1.5 rounded-md text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title="Regenerate response">
          <RotateCcw size={14} />
        </button>
      )}
      {onPin && (
        <button onClick={onPin} className={`p-1.5 rounded-md ${pinned ? 'text-zinc-200 hover:text-zinc-100 bg-zinc-800/70' : 'text-zinc-500 hover:text-zinc-300'} hover:bg-zinc-800 transition-colors`} title={pinned ? 'Unpin message' : 'Pin message'}>
          <Pin size={14} />
        </button>
      )}
      {onDelete && (
        <button onClick={onDelete} className="p-1.5 rounded-md text-zinc-500 hover:text-red-400 hover:bg-zinc-800 transition-colors" title="Delete message">
          <Trash2 size={14} />
        </button>
      )}
    </div>
  )
}

// ── Image lightbox context ───────────────────────────────────────────────

interface LightboxCtx {
  open: (blobUrl: string) => void
}

const LightboxContext = createContext<LightboxCtx>({ open: () => {} })

// ── File preview modal context ─────────────────────────────────────────

import { FilePreviewModal, type PreviewItem } from './FilePreviewModal'

interface PreviewCtx {
  open: (item: PreviewItem) => void
}

const PreviewContext = createContext<PreviewCtx>({ open: () => {} })

// ── Search context ─────────────────────────────────────────────────────

const SearchContext = createContext<string>('')
const PINNED_MESSAGES_KEY = 'myclaw_pinned_messages'

import { SearchBar } from './SearchBar'

// ── Editable user bubble ───────────────────────────────────────────────

function EditableUserBubble({ content, images, files, onResend, onDelete, onPin, pinned }: {
  content: string; images?: { path: string; mime?: string; name?: string }[]; files?: { path: string; mime?: string; name?: string }[]
  onResend?: (text: string) => void; onDelete?: () => void; onPin?: () => void; pinned?: boolean; msgId?: string
}) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(content)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const segments = useMemo(() => splitSystemReminders(content), [content])
  const hasAttachments = !!((images && images.length > 0) || (files && files.length > 0))

  useEffect(() => { if (editing) { setDraft(content); setTimeout(() => textareaRef.current?.focus(), 0) } }, [editing, content])

  const handleSave = () => {
    const trimmed = draft.trim()
    if (trimmed && trimmed !== content && onResend) onResend(trimmed)
    setEditing(false)
  }

  return (
    <div className="flex justify-end gap-2.5 sm:gap-3.5 group/msg">
      <div className="max-w-[85%] sm:max-w-[78%] lg:max-w-[72%] rounded-2xl border border-zinc-700/40 bg-zinc-800/60 px-3 sm:px-4 lg:px-5 py-3 sm:py-4 text-sm text-zinc-100 leading-relaxed shadow-sm space-y-3 transition-colors hover:border-zinc-600/50">
        {images && images.length > 0 && (
          <div className="flex flex-wrap gap-2 mb-2">
            {images.map((img, i) => <LazyImage key={i} path={img.path} mime={img.mime} name={img.name} />)}
          </div>
        )}
        {files && files.length > 0 && (
          <div className="flex flex-col gap-2 mb-2">
            {files.map((f, i) => renderFileRef(f, i))}
          </div>
        )}
        {editing ? (
          <div className="space-y-2">
            <textarea
              ref={textareaRef}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleSave() } if (e.key === 'Escape') setEditing(false) }}
              className="w-full bg-zinc-900 border border-zinc-700 rounded-lg px-3 py-2 text-sm text-zinc-100 outline-none focus:border-zinc-500 resize-none min-h-[60px]"
              rows={3}
            />
            <div className="flex items-center gap-2 justify-end">
              {hasAttachments && <span className="mr-auto text-[10px] text-zinc-500">Attachments won't be resent</span>}
              <button onClick={() => setEditing(false)} className="px-2 py-1 text-xs text-zinc-400 hover:text-zinc-200 rounded-lg hover:bg-zinc-700 transition-colors">Cancel</button>
              <button onClick={handleSave} className="px-3 py-1 text-xs text-zinc-200 hover:text-zinc-50 rounded-lg hover:bg-zinc-700 transition-colors font-medium">Send</button>
            </div>
          </div>
        ) : (
          <>
          <SearchContext.Consumer>{(q) => (
            <div className="space-y-1.5">
              {segments.map((segment, index) => segment.type === 'system-reminder'
                ? <SystemReminderCard key={`sys-${index}`} text={segment.text} />
                : segment.text ? <div key={`text-${index}`} className="whitespace-pre-wrap">{highlightText(segment.text, q)}</div> : null
              )}
            </div>
          )}</SearchContext.Consumer>
          {/* Edit/Delete actions */}
          <div className="flex items-center gap-0.5 justify-end mt-1">
              {onResend && (
                <button onClick={() => setEditing(true)} className="p-1.5 rounded-md text-zinc-500 hover:text-zinc-300 hover:bg-zinc-700 transition-colors" title="Edit & resend">
                  <Pencil size={14} />
                </button>
              )}
              {onPin && (
                <button onClick={onPin} className={`p-1.5 rounded-md ${pinned ? 'text-zinc-200 hover:text-zinc-100 bg-zinc-700/70' : 'text-zinc-500 hover:text-zinc-300'} hover:bg-zinc-700 transition-colors`} title={pinned ? 'Unpin message' : 'Pin message'}>
                  <Pin size={14} />
                </button>
              )}
              {onDelete && (
                <button onClick={onDelete} className="p-1.5 rounded-md text-zinc-500 hover:text-red-400 hover:bg-zinc-700 transition-colors" title="Delete message">
                  <Trash2 size={14} />
                </button>
              )}
          </div>
          </>
        )}
      </div>
      <div className="mt-0.5 h-8 w-8 sm:h-10 sm:w-10 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-lg sm:text-xl shrink-0 select-none shadow-md">👤</div>
    </div>
  )
}

// ── Memoized bubbles ─────────────────────────────────────────────────────

const UserBubble = memo(function UserBubble({ content, images, files, onResend, onDelete, onPin, pinned, msgId }: {
  content: string; images?: { path: string; mime?: string; name?: string }[]; files?: { path: string; mime?: string; name?: string }[]
  onResend?: (text: string) => void; onDelete?: () => void; onPin?: () => void; pinned?: boolean; msgId?: string
}) {
  return <EditableUserBubble content={content} images={images} files={files} onResend={onResend} onDelete={onDelete} onPin={onPin} pinned={pinned} msgId={msgId} />
})

interface AssistantBubbleProps {
  blocks: MessageBlock[]
  done: boolean
  isLast: boolean
  isGenerating: boolean
  onRetry?: () => void
  onDelete?: () => void
  onPin?: () => void
  pinned?: boolean
}

const AssistantBubble = memo(function AssistantBubble({ blocks, done, isLast, isGenerating, onRetry, onDelete, onPin, pinned }: AssistantBubbleProps) {
  return (
    <div className="flex gap-2.5 sm:gap-3.5 group/msg">
      <div className="mt-0.5 h-8 w-8 sm:h-10 sm:w-10 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-lg sm:text-xl shrink-0 select-none shadow-md">🦀</div>
      <div className={`flex-1 min-w-0 rounded-2xl border bg-zinc-900/40 px-3 sm:px-4 lg:px-5 py-3 sm:py-4 space-y-3 shadow-sm transition-colors ${isGenerating ? 'generating-border' : 'border-zinc-800/80 hover:border-zinc-800'}`}>
        {blocks.map((block, i) => renderBlock(block, i, isGenerating))}
        {isGenerating && blocks.length === 0 && <GeneratingDots />}
        {done && <MessageActions blocks={blocks} isLast={isLast} isGenerating={isGenerating} onRetry={onRetry} onDelete={onDelete} onPin={onPin} pinned={pinned} />}
      </div>
    </div>
  )
})

// ── MessageList with virtualization ──────────────────────────────────────

interface Props {
  messages: ChatMessage[]
  containerRef: RefObject<HTMLDivElement | null>
  onRetry?: (userContent: string) => void
}

export default function MessageList({ messages, containerRef, onRetry }: Props) {
  // Preload KaTeX CSS on mount so math renders without a flash
  useEffect(() => {
    if (isRehypeKatexLoaded()) ensureKatexCss()
  }, [])
  const { sendMessage, setMessages, isGenerating: globalGenerating, request } = useWebSocketContext()
  const { toast } = useToast()
  const [isNearBottom, setIsNearBottom] = useState(true)
  const [isNearTop, setIsNearTop] = useState(true)
  const scrollElementRef = useRef<HTMLDivElement>(null)
  // Track stick-to-bottom intent via ref to avoid race conditions during streaming
  const stickToBottomRef = useRef(true)
  const lastScrollTopRef = useRef(0)
  const lastAssistantIdx = messages.reduce((acc, m, i) => (m.role === 'assistant' ? i : acc), -1)

  // Lightbox state
  const [lightboxOpen, setLightboxOpen] = useState(false)
  const [lightboxIndex, setLightboxIndex] = useState(0)

  // File preview state
  const [previewItem, setPreviewItem] = useState<PreviewItem | null>(null)

  // Search state
  const [searchOpen, setSearchOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const [matchIdx, setMatchIdx] = useState(0)
  const [pinnedIds, setPinnedIds] = useState<string[]>(() => {
    try { return JSON.parse(localStorage.getItem(PINNED_MESSAGES_KEY) || '[]') } catch { return [] }
  })

  // Compute matching message indices
  const matchIndices = searchQuery
    ? messages.reduce<number[]>((acc, msg, i) => {
        const content = searchableMessageText(msg)
        if (content.toLowerCase().includes(searchQuery.toLowerCase())) acc.push(i)
        return acc
      }, [])
    : []
  const currentMatchIndex = matchIndices.length ? matchIndices[Math.min(matchIdx, matchIndices.length - 1)] : -1
  const pinnedMessages = pinnedIds.map(id => messages.find(m => m.id === id)).filter(Boolean) as ChatMessage[]
  const slides = useMemo(() => {
    const seen = new Set<string>()
    const cache = getImageCache()
    return messages.flatMap((msg) => {
      if (msg.role !== 'user') return []
      return (msg.images ?? []).map((img) => cache.get(img.path)).filter((url): url is string => !!url)
    }).filter((url) => {
      if (seen.has(url)) return false
      seen.add(url)
      return true
    }).map((url) => ({ src: url }))
  }, [messages])

  useEffect(() => {
    if (!previewItem && !lightboxOpen) releaseUnusedBlobUrls(messages, slides.map((slide) => slide.src))
  }, [messages, previewItem, lightboxOpen, slides])

  useEffect(() => { setMatchIdx(0) }, [searchQuery])


  useEffect(() => { localStorage.setItem(PINNED_MESSAGES_KEY, JSON.stringify(pinnedIds)) }, [pinnedIds])

  const togglePin = useCallback((id: string) => {
    setPinnedIds(prev => {
      const pinned = prev.includes(id)
      toast(pinned ? 'Message unpinned' : 'Message pinned', 'info')
      return pinned ? prev.filter(x => x !== id) : [...prev, id]
    })
  }, [toast])

  // Ctrl+F handler
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === 'f') {
        const target = e.target as HTMLElement | null
        if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) return
        e.preventDefault()
        setSearchOpen(true)
      }
      if (e.key === 'Escape' && searchOpen) {
        setSearchOpen(false)
        setSearchQuery('')
        setMatchIdx(0)
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [searchOpen])

  // Delete message handler (removes from local view; history reload may restore it)
  const handleDelete = useCallback((msgId: string) => {
    setMessages((prev) => prev.filter((m) => m.id !== msgId))
    setPinnedIds((prev) => prev.filter((id) => id !== msgId))
    toast('Message hidden locally. Reloaded history may restore it.', 'info')
  }, [setMessages, toast])

  // Resend edited message handler
  const handleResend = useCallback((_original: string, edited: string) => {
    sendMessage(edited)
  }, [sendMessage])

  const openLightbox = useCallback((blobUrl: string) => {
    const idx = slides.findIndex((slide) => slide.src === blobUrl)
    if (idx >= 0) {
      setLightboxIndex(idx)
      setLightboxOpen(true)
    }
  }, [slides])

  const lightboxCtx = useRef<LightboxCtx>({ open: openLightbox })
  lightboxCtx.current = { open: openLightbox }

  const previewCtx = useRef<PreviewCtx>({ open: (item) => setPreviewItem(item) })
  previewCtx.current = { open: (item) => setPreviewItem(item) }

  const virtualizer = useVirtualizer({
    count: messages.length,
    getScrollElement: () => scrollElementRef.current,
    estimateSize: () => 120, // rough estimate for variable-height messages
    overscan: 5,
    measureElement: (el) => el.getBoundingClientRect().height,
  })

  useEffect(() => {
    if (currentMatchIndex >= 0) {
      virtualizer.scrollToIndex(currentMatchIndex, { align: 'center', behavior: 'smooth' })
      stickToBottomRef.current = false
    }
  }, [currentMatchIndex, virtualizer])

  const checkNearBottom = useCallback(() => {
    const el = scrollElementRef.current
    if (!el) return
    const { scrollTop, scrollHeight, clientHeight } = el
    const gap = scrollHeight - scrollTop - clientHeight
    // Distinguish user scroll-up (scrollTop decreased) from content growth (scrollTop same, scrollHeight grew).
    // Only unstick when the user intentionally scrolls up.
    if (scrollTop < lastScrollTopRef.current - 2 && gap > 120) {
      stickToBottomRef.current = false
    }
    // Re-stick when back near the bottom
    if (gap < 60) {
      stickToBottomRef.current = true
    }
    lastScrollTopRef.current = scrollTop
    setIsNearBottom(stickToBottomRef.current)
    setIsNearTop(scrollTop < 200)
  }, [])

  useEffect(() => {
    const el = scrollElementRef.current
    if (!el) return
    el.addEventListener('scroll', checkNearBottom, { passive: true })
    return () => el.removeEventListener('scroll', checkNearBottom)
  }, [checkNearBottom])

  // Auto-scroll when near bottom — triggers on new messages AND content growth.
  // Track total content length of the last assistant message to detect streaming updates.
  const lastMsg = messages[messages.length - 1]
  const lastContentLen = lastMsg?.role === 'assistant'
    ? lastMsg.blocks.reduce((n, b) => n + (b.type === 'content' ? b.text.length : b.type === 'thinking' ? b.text.length : 0), 0)
    : 0

  useEffect(() => {
    if (stickToBottomRef.current) {
      // Use requestAnimationFrame so the DOM has been painted with new content before scrolling
      requestAnimationFrame(() => {
        const el = scrollElementRef.current
        if (el) el.scrollTop = el.scrollHeight
      })
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messages.length, lastContentLen])

  const scrollToBottom = () => {
    stickToBottomRef.current = true
    setIsNearBottom(true)
    virtualizer.scrollToIndex(messages.length - 1, { align: 'end', behavior: 'smooth' })
  }

  const scrollToTop = () => {
    stickToBottomRef.current = false
    virtualizer.scrollToIndex(0, { align: 'start', behavior: 'smooth' })
  }

  if (messages.length === 0) {
    return <div ref={containerRef} className="flex-1 overflow-y-auto" />
  }

  const virtualItems = virtualizer.getVirtualItems()

  return (
    <ChatErrorBoundary>
    <SearchContext.Provider value={searchQuery}>
    <FileRequestContext.Provider value={{ request }}>
    <LightboxContext.Provider value={lightboxCtx.current}>
    <PreviewContext.Provider value={previewCtx.current}>
      <div className="flex-1 relative">
        {pinnedMessages.length > 0 && (
          <div className="absolute top-2 left-2 right-2 sm:left-4 sm:right-4 z-10 flex gap-2 overflow-x-auto pointer-events-auto">
            {pinnedMessages.map((m) => {
              const text = m.role === 'user' ? m.content : extractText(m.blocks)
              return (
                <button key={m.id} onClick={() => virtualizer.scrollToIndex(messages.findIndex(x => x.id === m.id), { align: 'center', behavior: 'smooth' })} className="flex items-center gap-1.5 max-w-[220px] rounded-full border border-zinc-700/70 bg-zinc-900/90 px-3 py-1 text-xs text-zinc-300 shadow-lg hover:bg-zinc-800 transition-colors" title="Jump to pinned message">
                  <Pin size={11} className="text-zinc-400 shrink-0" />
                  <span className="truncate">{text}</span>
                </button>
              )
            })}
          </div>
        )}
        {searchOpen && (
          <SearchBar
            query={searchQuery}
            setQuery={setSearchQuery}
            matchCount={matchIndices.length}
            matchIdx={matchIdx < matchIndices.length ? matchIdx : 0}
            onPrev={() => setMatchIdx((i) => matchIndices.length ? (i - 1 + matchIndices.length) % matchIndices.length : 0)}
            onNext={() => setMatchIdx((i) => matchIndices.length ? (i + 1) % matchIndices.length : 0)}
            onClose={() => { setSearchOpen(false); setSearchQuery(''); setMatchIdx(0) }}
          />
        )}
        <div ref={(el) => { scrollElementRef.current = el; if (containerRef) (containerRef as any).current = el }} className="absolute inset-0 overflow-y-auto">
          <div style={{ height: virtualizer.getTotalSize(), width: '100%', position: 'relative' }}>
            <div style={{ position: 'absolute', top: 0, left: 0, width: '100%', transform: `translateY(${virtualItems[0]?.start ?? 0}px)` }}>
              {virtualItems.map((vi) => {
                const msg = messages[vi.index]
                return (
                  <div key={vi.key} data-index={vi.index} ref={(el) => { if (el) virtualizer.measureElement(el) }} className={`px-3 sm:px-8 py-3 sm:py-4 rounded-2xl transition-colors ${vi.index === currentMatchIndex ? 'bg-amber-500/10 ring-1 ring-amber-500/30' : ''}`}>
                    {shouldShowTimeDivider(messages[vi.index - 1], msg) && (
                      <div className="flex items-center gap-3 my-2 text-xs text-zinc-600">
                        <div className="h-px flex-1 bg-zinc-800" />
                        <span>{timeDividerLabel(messageTimestamp(msg.id)!)}</span>
                        <div className="h-px flex-1 bg-zinc-800" />
                      </div>
                    )}
                    {msg.role === 'user' ? (
                      <UserBubble
                        content={msg.content}
                        images={'images' in msg ? (msg as any).images : undefined}
                        files={'files' in msg ? (msg as any).files : undefined}
                        onResend={(edited) => handleResend(msg.content, edited)}
                        onDelete={() => handleDelete(msg.id)}
                        onPin={() => togglePin(msg.id)}
                        pinned={pinnedIds.includes(msg.id)}
                        msgId={msg.id}
                      />
                    ) : (
                      (() => {
                        const isLast = vi.index === lastAssistantIdx
                        const prevUser = isLast ? [...messages.slice(0, vi.index)].reverse().find((m) => m.role === 'user') : undefined
                        return (
                          <AssistantBubble
                            blocks={msg.blocks}
                            done={msg.done}
                            isLast={isLast}
                            isGenerating={!msg.done && globalGenerating}
                            onRetry={onRetry && prevUser ? () => onRetry((prevUser as { content: string }).content) : undefined}
                            onDelete={() => handleDelete(msg.id)}
                            onPin={() => togglePin(msg.id)}
                            pinned={pinnedIds.includes(msg.id)}
                          />
                        )
                      })()
                    )}
                  </div>
                )
              })}
            </div>
          </div>
        </div>

        {!isNearBottom && (
          <button onClick={scrollToBottom} className="absolute bottom-4 left-1/2 -translate-x-1/2 flex items-center gap-1.5 px-3 py-1.5 rounded-full bg-zinc-800 border border-zinc-700 text-xs text-zinc-300 hover:bg-zinc-700 shadow-lg transition-colors z-10">
            <ArrowDown size={12} /><span>Scroll to bottom</span>
          </button>
        )}
        {!isNearTop && messages.length > 20 && (
          <button onClick={scrollToTop} className="absolute top-4 left-1/2 -translate-x-1/2 flex items-center gap-1.5 px-3 py-1.5 rounded-full bg-zinc-800 border border-zinc-700 text-xs text-zinc-300 hover:bg-zinc-700 shadow-lg transition-colors z-10">
            <ArrowUp size={12} /><span>Scroll to top</span>
          </button>
        )}

        <Lightbox
          open={lightboxOpen}
          close={() => setLightboxOpen(false)}
          index={lightboxIndex}
          slides={slides}
          plugins={[Zoom, Download, Fullscreen]}
          zoom={{
            maxZoomPixelRatio: 5,
            zoomInMultiplier: 2,
          }}
          carousel={{ finite: slides.length <= 1 }}
          animation={{ fade: 300 }}
          controller={{ closeOnBackdropClick: true }}
          styles={{
            container: { backgroundColor: 'rgba(0, 0, 0, 0.85)' },
          }}
        />
        {previewItem && <FilePreviewModal item={previewItem} onClose={() => setPreviewItem(null)} />}
      </div>
    </PreviewContext.Provider>
    </LightboxContext.Provider>
    </FileRequestContext.Provider>
    </SearchContext.Provider>
    </ChatErrorBoundary>
  )
}
