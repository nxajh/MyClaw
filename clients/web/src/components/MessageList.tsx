import { useState, useEffect, useRef, useCallback, type ReactNode } from 'react'
import type { RefObject } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import rehypeHighlight from 'rehype-highlight'
import { ChevronDown, ChevronRight, Copy, Check, ArrowDown, RotateCcw } from 'lucide-react'
import type { ChatMessage, MessageBlock } from '../hooks/useWebSocket'
import ToolCallCard from './ToolCallCard'

// ── PreCodeBlock with syntax highlighting + language label ────────────────

function PreCodeBlock({ children, className }: { children: ReactNode; className?: string }) {
  const [copied, setCopied] = useState(false)
  const preRef = useRef<HTMLPreElement>(null)

  // Extract language from rehype-highlight's class="language-xxx" on <code>
  const lang = className?.replace('language-', '') || ''

  const handleCopy = async () => {
    if (!preRef.current) return
    const text = preRef.current.innerText || ''
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch {
      // ignore
    }
  }

  return (
    <div className="relative group/code my-4 overflow-hidden rounded-xl border border-zinc-850 bg-zinc-950 shadow-md">
      <div className="flex items-center justify-between px-4 py-1.5 border-b border-zinc-900 bg-zinc-900/40 text-[10px] text-zinc-500 font-mono select-none">
        <span>{lang || 'code'}</span>
        <button
          onClick={handleCopy}
          className="flex items-center gap-1 px-2 py-0.5 rounded bg-zinc-900 hover:bg-zinc-850 hover:text-zinc-200 border border-zinc-800/60 opacity-60 group-hover/code:opacity-100 transition-opacity"
        >
          {copied ? (
            <>
              <Check size={10} className="text-emerald-400" />
              <span className="text-emerald-400">Copied</span>
            </>
          ) : (
            <>
              <Copy size={10} />
              <span>Copy</span>
            </>
          )}
        </button>
      </div>
      <pre ref={preRef} className="p-4 overflow-x-auto text-xs leading-6 text-zinc-350 focus:outline-none !my-0 !bg-transparent !border-none">
        {children}
      </pre>
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

// ── Thinking block (P1-2: with Markdown rendering) ───────────────────────

function ThinkingBlock({ text, defaultOpen }: { text: string; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen ?? false)

  useEffect(() => {
    if (defaultOpen) {
      setOpen(true)
    }
  }, [defaultOpen])

  return (
    <div className="rounded-xl border border-zinc-800 overflow-hidden text-xs">
      <button
        onClick={() => setOpen(!open)}
        className="w-full flex items-center gap-2 px-3.5 py-2.5 text-left text-zinc-500 hover:text-zinc-400 hover:bg-zinc-800/30 transition-colors"
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        <span>Thinking</span>
      </button>
      {open && (
        <div className="px-3.5 py-3 text-zinc-500 leading-5 border-t border-zinc-800 bg-zinc-900/40
          prose prose-invert prose-sm max-w-none prose-p:my-1 prose-li:my-0
          prose-code:text-zinc-400 prose-code:bg-zinc-800/60 prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
          prose-strong:text-zinc-400 prose-a:text-blue-400/70">
          <Markdown
            remarkPlugins={[remarkGfm]}
            components={{
              pre: ({ children }) => <pre className="!bg-transparent !p-0 !my-1 overflow-x-auto">{children}</pre>,
            }}
          >
            {text}
          </Markdown>
        </div>
      )}
    </div>
  )
}

// ── Content renderer (streaming plain text vs. full Markdown) ────────────

function ContentBlock({ text, done }: { text: string; done: boolean }) {
  const proseClasses = `prose prose-invert prose-sm max-w-none
    prose-p:leading-7 prose-p:my-2 first:prose-p:mt-0
    prose-headings:text-zinc-100 prose-headings:font-semibold prose-headings:mt-5 prose-headings:mb-2
    prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded-md prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
    prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400
    prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline
    prose-strong:text-zinc-200 prose-strong:font-semibold
    prose-ul:my-2 prose-ol:my-2 prose-li:my-0.5
    prose-hr:border-zinc-800`

  // P0-3: During streaming, render as plain text to avoid O(n²) Markdown
  // re-parsing on every token chunk.  Switch to full Markdown once done.
  if (!done) {
    return (
      <div className={`${proseClasses} whitespace-pre-wrap`}>
        {text}
      </div>
    )
  }

  return (
    <div className={proseClasses}>
      <Markdown
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={[rehypeKatex, rehypeHighlight]}
        components={{
          pre: ({ children, ...props }) => {
            // Extract language class from the <code> child for the header label.
            const codeChild = Array.isArray(children)
              ? children.find((c: any) => c?.props?.className?.includes('language-'))
              : (children as any)?.props?.className?.includes('language-') ? children : null
            const codeClassName = codeChild?.props?.className || ''
            return <PreCodeBlock className={codeClassName}>{children}</PreCodeBlock>
          },
        }}
      >
        {text}
      </Markdown>
    </div>
  )
}

// ── Block renderer ───────────────────────────────────────────────────────

function renderBlock(block: MessageBlock, index: number, isGenerating: boolean) {
  if (block.type === 'content') {
    return <ContentBlock key={index} text={block.text} done={!isGenerating} />
  }
  if (block.type === 'thinking') {
    return <ThinkingBlock key={index} text={block.text} defaultOpen={isGenerating} />
  }
  // tool_call
  return <ToolCallCard key={block.id} block={block} />
}

// ── Message actions (P1-1: copy / retry) ─────────────────────────────────

function extractText(blocks: MessageBlock[]): string {
  return blocks
    .filter((b): b is { type: 'content'; text: string } => b.type === 'content')
    .map((b) => b.text)
    .join('\n\n')
}

function MessageActions({
  blocks,
  isLast,
  isGenerating,
  onRetry,
}: {
  blocks: MessageBlock[]
  isLast: boolean
  isGenerating: boolean
  onRetry?: () => void
}) {
  const [copied, setCopied] = useState(false)

  const handleCopy = async () => {
    const text = extractText(blocks)
    if (!text) return
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch {
      // ignore
    }
  }

  return (
    <div className="flex items-center gap-1 mt-1 opacity-0 group-hover/msg:opacity-100 transition-opacity">
      <button
        onClick={handleCopy}
        className="flex items-center gap-1 px-1.5 py-1 rounded-md text-[11px] text-zinc-600 hover:text-zinc-300 hover:bg-zinc-800 transition-colors"
        title="Copy message"
      >
        {copied ? <Check size={12} className="text-emerald-400" /> : <Copy size={12} />}
        <span>{copied ? 'Copied' : 'Copy'}</span>
      </button>
      {isLast && !isGenerating && onRetry && (
        <button
          onClick={onRetry}
          className="flex items-center gap-1 px-1.5 py-1 rounded-md text-[11px] text-zinc-600 hover:text-zinc-300 hover:bg-zinc-800 transition-colors"
          title="Regenerate response"
        >
          <RotateCcw size={12} />
          <span>Retry</span>
        </button>
      )}
    </div>
  )
}

// ── MessageList ──────────────────────────────────────────────────────────

interface Props {
  messages: ChatMessage[]
  containerRef: RefObject<HTMLDivElement | null>
  onRetry?: (userContent: string) => void
}

export default function MessageList({ messages, containerRef, onRetry }: Props) {
  const [isNearBottom, setIsNearBottom] = useState(true)

  // Track whether user is scrolled near the bottom.
  const checkNearBottom = useCallback(() => {
    const el = containerRef.current
    if (!el) return
    const threshold = 120
    setIsNearBottom(el.scrollHeight - el.scrollTop - el.clientHeight < threshold)
  }, [containerRef])

  // Attach scroll listener.
  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    el.addEventListener('scroll', checkNearBottom, { passive: true })
    return () => el.removeEventListener('scroll', checkNearBottom)
  }, [containerRef, checkNearBottom])

  // Auto-scroll only when user is near the bottom.
  useEffect(() => {
    if (isNearBottom) {
      const el = containerRef.current
      if (el) el.scrollTop = el.scrollHeight
    }
  }, [messages, isNearBottom, containerRef])

  const scrollToBottom = () => {
    const el = containerRef.current
    if (el) el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' })
  }

  if (messages.length === 0) {
    return (
      <div ref={containerRef} className="flex-1 overflow-y-auto flex flex-col items-center justify-center gap-4 px-6 text-center">
        <div className="text-5xl select-none">🦀</div>
        <div>
          <h2 className="text-lg font-semibold text-zinc-200 mb-1">How can I help?</h2>
          <p className="text-sm text-zinc-500 max-w-xs">
            Ask me anything — I can use tools, write code, search, and more.
          </p>
        </div>
      </div>
    )
  }

  // Find the last assistant message index for retry targeting.
  const lastAssistantIdx = messages.reduce(
    (acc, m, i) => (m.role === 'assistant' ? i : acc),
    -1,
  )

  return (
    <div className="flex-1 relative">
      <div ref={containerRef} className="absolute inset-0 overflow-y-auto">
        <div className="max-w-3xl mx-auto px-4 py-8 space-y-8">
          {messages.map((msg, msgIdx) => {
            if (msg.role === 'user') {
              return (
                <div key={msg.id} className="flex justify-end gap-3.5">
                  <div className="max-w-[78%] rounded-2xl rounded-tr-lg bg-zinc-800 px-5 py-3.5 text-sm text-zinc-100 whitespace-pre-wrap leading-relaxed">
                    {msg.content}
                  </div>
                  {/* Avatar */}
                  <div className="mt-0.5 h-7 w-7 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-base shrink-0 select-none shadow-md">
                    👤
                  </div>
                </div>
              )
            }

            // Assistant
            const isLast = msgIdx === lastAssistantIdx
            // Find the preceding user message for retry.
            const prevUser = isLast
              ? [...messages.slice(0, msgIdx)].reverse().find((m) => m.role === 'user')
              : undefined

            return (
              <div key={msg.id} className="flex gap-3.5 group/msg">
                {/* Avatar */}
                <div className="mt-0.5 h-7 w-7 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-base shrink-0 select-none shadow-md">
                  🦀
                </div>

                <div className="flex-1 min-w-0 rounded-2xl border border-zinc-800/80 bg-zinc-900/25 px-5 py-4 space-y-3 shadow-sm hover:border-zinc-800 transition-colors">
                  {msg.blocks.map((block, i) => renderBlock(block, i, !msg.done))}

                  {/* Generating indicator — shown only before any content arrives */}
                  {!msg.done && msg.blocks.length === 0 && <GeneratingDots />}

                  {/* P1-1: Message actions — visible on hover */}
                  {msg.done && (
                    <MessageActions
                      blocks={msg.blocks}
                      isLast={isLast}
                      isGenerating={!msg.done}
                      onRetry={
                        onRetry && prevUser
                          ? () => onRetry((prevUser as { content: string }).content)
                          : undefined
                      }
                    />
                  )}
                </div>
              </div>
            )
          })}
        </div>
      </div>

      {/* Scroll-to-bottom button */}
      {!isNearBottom && (
        <button
          onClick={scrollToBottom}
          className="absolute bottom-4 left-1/2 -translate-x-1/2 flex items-center gap-1.5 px-3 py-1.5 rounded-full bg-zinc-800 border border-zinc-700 text-xs text-zinc-300 hover:bg-zinc-700 shadow-lg transition-colors z-10"
        >
          <ArrowDown size={12} />
          <span>Scroll to bottom</span>
        </button>
      )}
    </div>
  )
}
