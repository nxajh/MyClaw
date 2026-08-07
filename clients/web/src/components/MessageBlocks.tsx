import { useState, useEffect, useRef, useMemo, memo, useContext, type ReactNode } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeHighlight from 'rehype-highlight'
import { ChevronDown, ChevronRight, Copy, Check } from 'lucide-react'
import type { MessageBlock } from '../hooks/useWebSocket'
import ToolCallCard from './ToolCallCard'
import { hasMath, normalizeMathDelimiters, closeUnclosedFences, ensureKatexCss, loadRehypeKatex, isRehypeKatexLoaded, getRehypeKatex } from '../lib/mathUtils'
import { highlightText } from '../lib/searchUtils'
import { SearchContext } from './MessageList'

// ── PreCodeBlock with syntax highlighting + language label ────────────────

export const PreCodeBlock = memo(function PreCodeBlock({ children, className }: { children: ReactNode; className?: string }) {
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
})

// ── Thinking block with throttled updates ────────────────────────────────

export const ThinkingBlock = memo(function ThinkingBlock({ text, isStreaming }: { text: string; isStreaming?: boolean }) {
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

export type ContentSegment = { type: 'text'; text: string } | { type: 'system-reminder'; text: string }

export function splitSystemReminders(text: string): ContentSegment[] {
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

export const SystemReminderCard = memo(function SystemReminderCard({ text }: { text: string }) {
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

export const ContentBlock = memo(function ContentBlock({ text, done }: { text: string; done: boolean }) {
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

export function renderBlock(block: MessageBlock, index: number, isGenerating: boolean) {
  if (block.type === 'content') return <ContentBlock key={index} text={block.text} done={!isGenerating} />
  if (block.type === 'thinking') return <ThinkingBlock key={index} text={block.text} isStreaming={isGenerating} />
  return <ToolCallCard key={block.id} block={block} />
}
