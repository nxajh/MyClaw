import { useState, useEffect, useRef, useCallback, useMemo, createContext, type ReactNode } from 'react'
import type { RefObject } from 'react'
import { ArrowDown, ArrowUp, Pin } from 'lucide-react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from './Toast'
import Lightbox from 'yet-another-react-lightbox'
import Zoom from 'yet-another-react-lightbox/plugins/zoom'
import Download from 'yet-another-react-lightbox/plugins/download'
import Fullscreen from 'yet-another-react-lightbox/plugins/fullscreen'
import 'yet-another-react-lightbox/styles.css'
import type { ChatMessage } from '../hooks/useWebSocket'
import { ensureKatexCss, isRehypeKatexLoaded } from '../lib/mathUtils'
import { searchableMessageText, messageTimestamp, timeDividerLabel, shouldShowTimeDivider } from '../lib/searchUtils'
import {
  releaseUnusedBlobUrls,
  getImageCache,
  FileRequestContext,
} from '../lib/fileUtils'
import { FilePreviewModal, type PreviewItem } from './FilePreviewModal'
import { SearchBar } from './SearchBar'
import { UserBubble, AssistantBubble, extractText } from './MessageBubbles'

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

// ── Image lightbox context ───────────────────────────────────────────────

interface LightboxCtx {
  open: (blobUrl: string) => void
}

const LightboxContext = createContext<LightboxCtx>({ open: () => {} })

// ── File preview modal context ─────────────────────────────────────────

interface PreviewCtx {
  open: (item: PreviewItem) => void
}

const PreviewContext = createContext<PreviewCtx>({ open: () => {} })

// ── Search context ─────────────────────────────────────────────────────

export const SearchContext = createContext<string>('')
const PINNED_MESSAGES_KEY = 'myclaw_pinned_messages'

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
