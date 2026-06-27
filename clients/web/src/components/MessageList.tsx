import { useState, useEffect, useRef, useCallback, memo, type ReactNode } from 'react'
import type { RefObject } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeHighlight from 'rehype-highlight'
import { ChevronDown, ChevronRight, Copy, Check, ArrowDown, RotateCcw } from 'lucide-react'
import { useVirtualizer } from '@tanstack/react-virtual'
import type { ChatMessage, MessageBlock } from '../hooks/useWebSocket'
import ToolCallCard from './ToolCallCard'

// Lazy-load KaTeX CSS only when math content is detected
let katexCssLoaded = false
function ensureKatexCss() {
  if (katexCssLoaded) return Promise.resolve()
  katexCssLoaded = true
  return import('katex/dist/katex.min.css')
}
const hasMath = (text: string) => /\$[^$]+\$|\$\$[^$]+\$\$/.test(text)
// Lazy-load rehype-katex only when needed
let RehypeKatex: any = null
async function loadRehypeKatex() {
  if (!RehypeKatex) {
    const mod = await import('rehype-katex')
    RehypeKatex = mod.default
  }
  return RehypeKatex
}

// ── PreCodeBlock with syntax highlighting + language label ────────────────

function PreCodeBlock({ children, className }: { children: ReactNode; className?: string }) {
  const [copied, setCopied] = useState(false)
  const preRef = useRef<HTMLPreElement>(null)
  const lang = className?.replace('language-', '') || ''

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
    <div className="relative group/code my-4 overflow-hidden rounded-xl border border-zinc-850 bg-zinc-950 shadow-md">
      <div className="flex items-center justify-between px-4 py-1.5 border-b border-zinc-900 bg-zinc-900/40 text-[10px] text-zinc-500 font-mono select-none">
        <span>{lang || 'code'}</span>
        <button onClick={handleCopy} className="flex items-center gap-1 px-2 py-0.5 rounded bg-zinc-900 hover:bg-zinc-850 hover:text-zinc-200 border border-zinc-800/60 opacity-60 group-hover/code:opacity-100 transition-opacity">
          {copied ? <><Check size={10} className="text-emerald-400" /><span className="text-emerald-400">Copied</span></> : <><Copy size={10} /><span>Copy</span></>}
        </button>
      </div>
      <pre ref={preRef} className="p-4 overflow-x-auto text-xs leading-6 text-zinc-350 focus:outline-none !my-0 !bg-transparent !border-none">{children}</pre>
    </div>
  )
}

// ── Generating dots ──────────────────────────────────────────────────────

function GeneratingDots() {
  return (
    <div className="flex items-center gap-1 h-5">
      <span className="h-1.5 w-1.5 rounded-full bg-zinc-500 animate-bounce [animation-delay:-0.3s]" />
      <span className="h-1.5 w-1.5 rounded-full bg-zinc-500 animate-bounce [animation-delay:-0.15s]" />
      <span className="h-1.5 w-1.5 rounded-full bg-zinc-500 animate-bounce" />
    </div>
  )
}

// ── Thinking block with throttled updates ────────────────────────────────

function ThinkingBlock({ text, defaultOpen }: { text: string; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen ?? false)
  const [displayText, setDisplayText] = useState(text)
  const rafRef = useRef<number | null>(null)
  const pendingTextRef = useRef(text)

  useEffect(() => { if (defaultOpen) setOpen(true) }, [defaultOpen])

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
    <div className="rounded-xl border border-zinc-800 overflow-hidden text-xs">
      <button onClick={() => setOpen(!open)} className="w-full flex items-center gap-2 px-3.5 py-2.5 text-left text-zinc-500 hover:text-zinc-400 hover:bg-zinc-800/30 transition-colors">
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}<span>Thinking</span>
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
}

// ── Content renderer ─────────────────────────────────────────────────────

function ContentBlock({ text, done }: { text: string; done: boolean }) {
  const [katexReady, setKatexReady] = useState(!!RehypeKatex)
  const needsMath = hasMath(text)

  useEffect(() => {
    if (done && needsMath && !katexReady) {
      Promise.all([ensureKatexCss(), loadRehypeKatex()]).then(() => setKatexReady(true))
    }
  }, [done, needsMath, katexReady])

  const proseClasses = `prose prose-invert prose-sm max-w-none prose-p:leading-7 prose-p:my-2 first:prose-p:mt-0 prose-headings:text-zinc-100 prose-headings:font-semibold prose-headings:mt-5 prose-headings:mb-2 prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded-md prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400 prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline prose-strong:text-zinc-200 prose-strong:font-semibold prose-ul:my-2 prose-ol:my-2 prose-li:my-0.5 prose-hr:border-zinc-800`

  if (!done) return <div className={`${proseClasses} whitespace-pre-wrap`}>{text}</div>

  const rehypePlugins: any[] = [rehypeHighlight]
  if (needsMath && katexReady && RehypeKatex) rehypePlugins.push(RehypeKatex)

  return (
    <div className={proseClasses}>
      <Markdown
        remarkPlugins={needsMath && katexReady ? [remarkGfm, remarkMath] : [remarkGfm]}
        rehypePlugins={rehypePlugins}
        components={{
          table: ({ children }) => <div className="overflow-x-auto my-2"><table className="border-collapse text-xs">{children}</table></div>,
          pre: ({ children }) => {
            const codeChild = Array.isArray(children) ? children.find((c: any) => c?.props?.className?.includes('language-')) : (children as any)?.props?.className?.includes('language-') ? children : null
            return <PreCodeBlock className={codeChild?.props?.className || ''}>{children}</PreCodeBlock>
          },
        }}
      >{text}</Markdown>
    </div>
  )
}

// ── Block renderer ───────────────────────────────────────────────────────

function renderBlock(block: MessageBlock, index: number, isGenerating: boolean) {
  if (block.type === 'content') return <ContentBlock key={index} text={block.text} done={!isGenerating} />
  if (block.type === 'thinking') return <ThinkingBlock key={index} text={block.text} defaultOpen={isGenerating} />
  return <ToolCallCard key={block.id} block={block} />
}

// ── Message actions ──────────────────────────────────────────────────────

function extractText(blocks: MessageBlock[]): string {
  return blocks.filter((b): b is { type: 'content'; text: string } => b.type === 'content').map((b) => b.text).join('\n\n')
}

function MessageActions({ blocks, isLast, isGenerating, onRetry }: { blocks: MessageBlock[]; isLast: boolean; isGenerating: boolean; onRetry?: () => void }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = async () => {
    const text = extractText(blocks)
    if (!text) return
    try { await navigator.clipboard.writeText(text); setCopied(true); setTimeout(() => setCopied(false), 2000) } catch { /* ignore */ }
  }
  return (
    <div className="flex items-center gap-1 mt-1 opacity-0 group-hover/msg:opacity-100 transition-opacity">
      <button onClick={handleCopy} className="flex items-center gap-1 px-1.5 py-1 rounded-md text-[11px] text-zinc-600 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title="Copy message">
        {copied ? <Check size={12} className="text-emerald-400" /> : <Copy size={12} />}<span>{copied ? 'Copied' : 'Copy'}</span>
      </button>
      {isLast && !isGenerating && onRetry && (
        <button onClick={onRetry} className="flex items-center gap-1 px-1.5 py-1 rounded-md text-[11px] text-zinc-600 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title="Regenerate response">
          <RotateCcw size={12} /><span>Retry</span>
        </button>
      )}
    </div>
  )
}

// ── Memoized bubbles ─────────────────────────────────────────────────────

const UserBubble = memo(function UserBubble({ content }: { content: string }) {
  return (
    <div className="flex justify-end gap-2.5 sm:gap-3.5">
      <div className="max-w-[85%] sm:max-w-[78%] rounded-2xl rounded-tr-lg bg-zinc-800 px-3.5 sm:px-5 py-2.5 sm:py-3.5 text-sm text-zinc-100 whitespace-pre-wrap leading-relaxed">{content}</div>
      <div className="mt-0.5 h-6 w-6 sm:h-7 sm:w-7 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-sm sm:text-base shrink-0 select-none shadow-md">👤</div>
    </div>
  )
})

interface AssistantBubbleProps {
  blocks: MessageBlock[]
  done: boolean
  isLast: boolean
  isGenerating: boolean
  onRetry?: () => void
}

const AssistantBubble = memo(function AssistantBubble({ blocks, done, isLast, isGenerating, onRetry }: AssistantBubbleProps) {
  return (
    <div className="flex gap-2.5 sm:gap-3.5 group/msg">
      <div className="mt-0.5 h-6 w-6 sm:h-7 sm:w-7 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-sm sm:text-base shrink-0 select-none shadow-md">🦀</div>
      <div className="flex-1 min-w-0 rounded-2xl border border-zinc-800/80 bg-zinc-900/25 px-3 sm:px-5 py-3 sm:py-4 space-y-3 shadow-sm hover:border-zinc-800 transition-colors">
        {blocks.map((block, i) => renderBlock(block, i, isGenerating))}
        {!isGenerating && blocks.length === 0 && <GeneratingDots />}
        {done && <MessageActions blocks={blocks} isLast={isLast} isGenerating={isGenerating} onRetry={onRetry} />}
      </div>
    </div>
  )
}, (prev, next) => (
  prev.blocks === next.blocks && prev.done === next.done && prev.isLast === next.isLast && prev.isGenerating === next.isGenerating && prev.onRetry === next.onRetry
))

// ── MessageList with virtualization ──────────────────────────────────────

interface Props {
  messages: ChatMessage[]
  containerRef: RefObject<HTMLDivElement | null>
  onRetry?: (userContent: string) => void
}

export default function MessageList({ messages, containerRef, onRetry }: Props) {
  const [isNearBottom, setIsNearBottom] = useState(true)
  const scrollElementRef = useRef<HTMLDivElement>(null)
  const lastAssistantIdx = messages.reduce((acc, m, i) => (m.role === 'assistant' ? i : acc), -1)

  const virtualizer = useVirtualizer({
    count: messages.length,
    getScrollElement: () => scrollElementRef.current,
    estimateSize: () => 120, // rough estimate for variable-height messages
    overscan: 5,
    measureElement: (el) => el.getBoundingClientRect().height,
  })

  const checkNearBottom = useCallback(() => {
    const el = scrollElementRef.current
    if (!el) return
    setIsNearBottom(el.scrollHeight - el.scrollTop - el.clientHeight < 120)
  }, [])

  useEffect(() => {
    const el = scrollElementRef.current
    if (!el) return
    el.addEventListener('scroll', checkNearBottom, { passive: true })
    return () => el.removeEventListener('scroll', checkNearBottom)
  }, [checkNearBottom])

  // Auto-scroll when near bottom
  useEffect(() => {
    if (isNearBottom) {
      virtualizer.scrollToIndex(messages.length - 1, { align: 'end' })
    }
  }, [messages.length, isNearBottom, virtualizer])

  const scrollToBottom = () => {
    virtualizer.scrollToIndex(messages.length - 1, { align: 'end', behavior: 'smooth' })
  }

  if (messages.length === 0) {
    return (
      <div ref={containerRef} className="flex-1 overflow-y-auto flex flex-col items-center justify-center gap-4 px-4 sm:px-6 text-center">
        <div className="text-5xl select-none">🦀</div>
        <div>
          <h2 className="text-lg font-semibold text-zinc-200 mb-1">How can I help?</h2>
          <p className="text-sm text-zinc-500 max-w-xs">Ask me anything — I can use tools, write code, search, and more.</p>
        </div>
      </div>
    )
  }

  const virtualItems = virtualizer.getVirtualItems()

  return (
    <div className="flex-1 relative">
      <div ref={(el) => { scrollElementRef.current = el; if (containerRef) (containerRef as any).current = el }} className="absolute inset-0 overflow-y-auto">
        <div style={{ height: virtualizer.getTotalSize(), width: '100%', position: 'relative' }}>
          <div style={{ position: 'absolute', top: 0, left: 0, width: '100%', transform: `translateY(${virtualItems[0]?.start ?? 0}px)` }}>
            {virtualItems.map((vi) => {
              const msg = messages[vi.index]
              return (
                <div key={vi.key} data-index={vi.index} ref={(el) => { if (el) virtualizer.measureElement(el) }} className="max-w-3xl mx-auto px-2 sm:px-4 py-3 sm:py-4">
                  {msg.role === 'user' ? (
                    <UserBubble content={msg.content} />
                  ) : (
                    (() => {
                      const isLast = vi.index === lastAssistantIdx
                      const prevUser = isLast ? [...messages.slice(0, vi.index)].reverse().find((m) => m.role === 'user') : undefined
                      return (
                        <AssistantBubble
                          blocks={msg.blocks}
                          done={msg.done}
                          isLast={isLast}
                          isGenerating={!msg.done}
                          onRetry={onRetry && prevUser ? () => onRetry((prevUser as { content: string }).content) : undefined}
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
    </div>
  )
}
