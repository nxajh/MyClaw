import { useState, useEffect, useRef, useCallback, memo, createContext, useContext, type ReactNode } from 'react'
import type { RefObject } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeHighlight from 'rehype-highlight'
import { ChevronDown, ChevronRight, Copy, Check, ArrowDown, RotateCcw } from 'lucide-react'
import { useVirtualizer } from '@tanstack/react-virtual'
import Lightbox from 'yet-another-react-lightbox'
import Zoom from 'yet-another-react-lightbox/plugins/zoom'
import Download from 'yet-another-react-lightbox/plugins/download'
import Fullscreen from 'yet-another-react-lightbox/plugins/fullscreen'
import 'yet-another-react-lightbox/styles.css'
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
        <button onClick={handleCopy} className="flex items-center gap-1 px-2 py-0.5 rounded bg-zinc-900 hover:bg-zinc-850 hover:text-zinc-200 border border-zinc-800/60 transition-opacity sm:opacity-60 sm:group-hover/code:opacity-100">
          {copied ? <><Check size={10} className="text-emerald-400" /><span className="text-emerald-400">Copied</span></> : <><Copy size={10} /><span>Copy</span></>}
        </button>
      </div>
      <pre ref={preRef} className="p-2 sm:p-3 lg:p-4 overflow-x-auto text-[11px] sm:text-xs leading-5 sm:leading-6 text-zinc-350 focus:outline-none !my-0 !bg-transparent !border-none">{children}</pre>
    </div>
  )
}

// ── Generating dots ──────────────────────────────────────────────────────

function GeneratingDots() {
  return (
    <div className="flex items-center gap-1.5 py-1">
      <span className="h-2 w-2 rounded-full bg-zinc-400 animate-bounce [animation-delay:-0.3s]" />
      <span className="h-2 w-2 rounded-full bg-zinc-400 animate-bounce [animation-delay:-0.15s]" />
      <span className="h-2 w-2 rounded-full bg-zinc-400 animate-bounce" />
    </div>
  )
}

// ── Thinking block with throttled updates ────────────────────────────────

function ThinkingBlock({ text, isStreaming }: { text: string; isStreaming?: boolean }) {
  const [open, setOpen] = useState(false)
  const [displayText, setDisplayText] = useState(text)
  const rafRef = useRef<number | null>(null)
  const pendingTextRef = useRef(text)
  const userToggled = useRef(false)

  // While streaming, auto-open on first content if user hasn't interacted.
  // When streaming ends, auto-collapse if user hasn't manually opened it.
  useEffect(() => {
    if (isStreaming && text && !userToggled.current) {
      setOpen(true)
    } else if (!isStreaming && !userToggled.current) {
      setOpen(false)
    }
  }, [isStreaming, text])

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

  const proseClasses = `prose prose-invert prose-sm lg:prose-base max-w-none prose-p:leading-6 sm:prose-p:leading-7 prose-p:my-2 first:prose-p:mt-0 prose-headings:text-zinc-100 prose-headings:font-semibold prose-headings:mt-5 prose-headings:mb-2 prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1 sm:prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded-md prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400 prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline prose-strong:text-zinc-200 prose-strong:font-semibold prose-ul:my-2 prose-ol:my-2 prose-li:my-0.5 prose-hr:border-zinc-800`

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
  if (block.type === 'thinking') return <ThinkingBlock key={index} text={block.text} isStreaming={isGenerating} />
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

// ── Image lightbox context ───────────────────────────────────────────────
// Module-level registry: tracks all loaded image blob URLs for the lightbox.

const imageCache = new Map<string, string>() // path -> blob URL

/** Ordered list of all registered slides (blob URLs). */
const slideRegistry: string[] = []
/** Map from blob URL to its position in slideRegistry. */
const slideIndex = new Map<string, number>()

function registerSlide(blobUrl: string): number {
  let idx = slideIndex.get(blobUrl)
  if (idx !== undefined) return idx
  idx = slideRegistry.length
  slideRegistry.push(blobUrl)
  slideIndex.set(blobUrl, idx)
  return idx
}

interface LightboxCtx {
  open: (blobUrl: string) => void
}

const LightboxContext = createContext<LightboxCtx>({ open: () => {} })

// ── File preview modal context ─────────────────────────────────────────

interface PreviewItem { src: string; mime: string; name: string }
interface PreviewCtx { open: (item: PreviewItem) => void }
const PreviewContext = createContext<PreviewCtx>({ open: () => {} })

function FilePreviewModal({ item, onClose }: { item: PreviewItem; onClose: () => void }) {
  const [zoom, setZoom] = useState(1)
  const isImage = item.mime.startsWith('image/')
  const isVideo = item.mime.startsWith('video/')
  const isAudio = item.mime.startsWith('audio/')
  const isPdf = item.mime === 'application/pdf'

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const handleZoomIn = () => setZoom((z) => Math.min(z + 0.25, 5))
  const handleZoomOut = () => setZoom((z) => Math.max(z - 0.25, 0.25))
  const handleResetZoom = () => setZoom(1)

  const handleDownload = () => {
    const a = document.createElement('a')
    a.href = item.src
    a.download = item.name
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
  }

  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-black/90" onClick={onClose}>
      {/* Header bar */}
      <div className="flex items-center justify-between px-4 py-2 bg-zinc-900/80 border-b border-zinc-800 shrink-0">
        <span className="text-sm text-zinc-300 truncate max-w-[60%]">{item.name}</span>
        <div className="flex items-center gap-2">
          {(isImage || isPdf) && (
            <>
              <button onClick={(e) => { e.stopPropagation(); handleZoomOut() }} className="p-1.5 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200 transition-colors" title="Zoom out">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/><line x1="8" y1="11" x2="14" y2="11"/></svg>
              </button>
              <button onClick={(e) => { e.stopPropagation(); handleResetZoom() }} className="px-1.5 py-1 text-[11px] text-zinc-500 hover:text-zinc-300 hover:bg-zinc-700 rounded transition-colors">
                {Math.round(zoom * 100)}%
              </button>
              <button onClick={(e) => { e.stopPropagation(); handleZoomIn() }} className="p-1.5 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200 transition-colors" title="Zoom in">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/><line x1="11" y1="8" x2="11" y2="14"/><line x1="8" y1="11" x2="14" y2="11"/></svg>
              </button>
            </>
          )}
          <button onClick={(e) => { e.stopPropagation(); handleDownload() }} className="p-1.5 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200 transition-colors" title="Download">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
          </button>
          <button onClick={onClose} className="p-1.5 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200 transition-colors" title="Close">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
          </button>
        </div>
      </div>
      {/* Content area */}
      <div className="flex-1 overflow-auto flex items-center justify-center p-4" onClick={(e) => e.stopPropagation()}>
        {isImage && (
          <img src={item.src} alt={item.name} style={{ transform: `scale(${zoom})` }} className="max-w-full max-h-full object-contain transition-transform" />
        )}
        {isVideo && (
          <video controls autoPlay src={item.src} className="max-w-full max-h-full rounded-lg" />
        )}
        {isAudio && (
          <div className="flex flex-col items-center gap-4">
            <span className="text-6xl">🎵</span>
            <audio controls autoPlay src={item.src} className="w-80" style={{ filter: 'invert(0.85) hue-rotate(180deg)' }} />
          </div>
        )}
        {isPdf && (
          <iframe src={item.src} title={item.name} style={{ transform: `scale(${zoom})`, transformOrigin: 'top center' }} className="w-full h-full border-0 rounded-lg" />
        )}
        {!isImage && !isVideo && !isAudio && !isPdf && (
          <div className="flex flex-col items-center gap-4 text-center">
            <span className="text-6xl">📄</span>
            <p className="text-zinc-300 text-sm">{item.name}</p>
            <p className="text-zinc-600 text-xs">此文件类型不支持预览</p>
            <button onClick={handleDownload} className="px-4 py-2 rounded-lg bg-zinc-800 border border-zinc-700 hover:border-zinc-500 text-sm text-zinc-300 transition-colors">
              下载文件
            </button>
          </div>
        )}
      </div>
    </div>
  )
}

/** Convert a data-URL (data:mime;base64,...) to a blob-URL. */
function dataUrlToBlobUrl(dataUrl: string): string {
  const [header, b64] = dataUrl.split(',')
  const mime = header.match(/data:(.*?);/)?.[1] || 'application/octet-stream'
  const bin = atob(b64)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  const blob = new Blob([bytes], { type: mime })
  return URL.createObjectURL(blob)
}

function LazyImage({ path, mime, name }: { path: string; mime?: string; name?: string }) {
  const [src, setSrc] = useState<string | null>(() => imageCache.get(path) ?? null)
  const [error, setError] = useState(false)
  const ctx = useContext(LightboxContext)

  useEffect(() => {
    if (imageCache.has(path)) {
      const cached = imageCache.get(path)!
      setSrc(cached)
      registerSlide(cached)
      return
    }
    // Use global request function exposed by useApi via window
    const fetchImage = async () => {
      try {
        const res = await (window as any).myclawRequest?.('file.read', { path }) as { data?: string; mime?: string } | undefined
        if (res?.data) {
          const mimeStr = res.mime || mime || 'image/png'
          const dataUrl = `data:${mimeStr};base64,${res.data}`
          const blobUrl = dataUrlToBlobUrl(dataUrl)
          imageCache.set(path, blobUrl)
          registerSlide(blobUrl)
          setSrc(blobUrl)
        } else {
          setError(true)
        }
      } catch {
        setError(true)
      }
    }
    fetchImage()
  }, [path, mime])

  if (error) {
    return <div className="text-xs text-zinc-600 italic">🖼️ {name || 'Image unavailable'}</div>
  }

  if (!src) {
    return (
      <div className="w-32 h-24 rounded-lg bg-zinc-800 border border-zinc-700 flex items-center justify-center">
        <div className="h-4 w-4 border-2 border-zinc-600 border-t-zinc-300 rounded-full animate-spin" />
      </div>
    )
  }

  return (
    <img
      src={src}
      alt={name || 'Attached image'}
      className="max-w-full max-h-48 sm:max-h-64 lg:max-h-80 rounded-lg border border-zinc-700 object-contain cursor-pointer hover:border-zinc-500 transition-colors"
      onClick={() => ctx.open(src)}
    />
  )
}

// ── Non-image file components (audio, video, PDF, generic) ──────────────

function AudioFileCard({ path, name }: { path: string; name?: string }) {
  const [src, setSrc] = useState<string | null>(() => imageCache.get(path) ?? null)
  const [error, setError] = useState(false)

  useEffect(() => {
    if (imageCache.has(path)) { setSrc(imageCache.get(path)!); return }
    const fetchFile = async () => {
      try {
        const res = await (window as any).myclawRequest?.('file.read', { path }) as { data?: string; mime?: string } | undefined
        if (res?.data) {
          const mimeStr = res.mime || 'audio/mpeg'
          const bin = atob(res.data)
          const bytes = new Uint8Array(bin.length)
          for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
          const blob = new Blob([bytes], { type: mimeStr })
          const blobUrl = URL.createObjectURL(blob)
          imageCache.set(path, blobUrl)
          setSrc(blobUrl)
        } else { setError(true) }
      } catch { setError(true) }
    }
    fetchFile()
  }, [path])

  if (error) return <div className="text-xs text-zinc-600 italic">🎵 {name || 'Audio unavailable'}</div>
  if (!src) return <div className="w-48 h-10 rounded-lg bg-zinc-800 border border-zinc-700 animate-pulse" />

  return (
    <div className="flex items-center gap-2 px-3 py-2 rounded-lg bg-zinc-800/60 border border-zinc-700">
      <span className="text-xs text-zinc-400 truncate max-w-[120px]">{name || 'Audio'}</span>
      <audio controls src={src} className="h-8 flex-1 min-w-0" style={{ filter: 'invert(0.85) hue-rotate(180deg)' }} />
    </div>
  )
}

function VideoFileCard({ path, name }: { path: string; name?: string }) {
  const [src, setSrc] = useState<string | null>(() => imageCache.get(path) ?? null)
  const [error, setError] = useState(false)
  const [playError, setPlayError] = useState(false)
  const [fileSize, setFileSize] = useState<number | null>(null)
  const [downloading, setDownloading] = useState(false)

  useEffect(() => {
    if (imageCache.has(path)) { setSrc(imageCache.get(path)!); return }
    const fetchFile = async () => {
      try {
        const res = await (window as any).myclawRequest?.('file.read', { path }) as { data?: string; mime?: string; size?: number } | undefined
        if (res?.data) {
          const mimeStr = res.mime || 'video/mp4'
          const bin = atob(res.data)
          const bytes = new Uint8Array(bin.length)
          for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
          const blob = new Blob([bytes], { type: mimeStr })
          const blobUrl = URL.createObjectURL(blob)
          imageCache.set(path, blobUrl)
          setSrc(blobUrl)
          setFileSize(res.size ?? null)
        } else { setError(true) }
      } catch { setError(true) }
    }
    fetchFile()
  }, [path])

  const handleDownload = async () => {
    if (!src) return
    setDownloading(true)
    try {
      const a = document.createElement('a')
      a.href = src
      a.download = name || 'video'
      document.body.appendChild(a)
      a.click()
      document.body.removeChild(a)
    } finally { setDownloading(false) }
  }

  if (error) return <div className="text-xs text-zinc-600 italic">🎬 {name || 'Video unavailable'}</div>
  if (!src) return (
    <div className="flex items-center gap-2 px-3 py-2 rounded-lg bg-zinc-800/60 border border-zinc-700">
      <div className="h-4 w-4 border-2 border-zinc-600 border-t-zinc-300 rounded-full animate-spin" />
      <span className="text-xs text-zinc-500">Loading video{name ? `: ${name}` : ''}…</span>
    </div>
  )

  // Browser can't play this codec (e.g. H.265/HEVC .mov) — show download card.
  if (playError) return (
    <button
      onClick={handleDownload}
      disabled={downloading}
      className="flex items-center gap-2 px-3 py-2 rounded-lg bg-zinc-800/60 border border-zinc-700 hover:border-zinc-500 transition-colors text-left"
    >
      <span className="text-lg">🎬</span>
      <div className="flex-1 min-w-0">
        <div className="text-xs text-zinc-300 truncate">{name || 'Video'}</div>
        <div className="text-[10px] text-zinc-600">
          {fileSize != null ? `${(fileSize / 1048576).toFixed(1)} MB · ` : ''}浏览器不支持此编码，请下载播放
        </div>
      </div>
      {downloading && <div className="h-3 w-3 border-2 border-zinc-600 border-t-zinc-300 rounded-full animate-spin" />}
    </button>
  )

  return (
    <div className="flex flex-col gap-1">
      <video
        controls
        src={src}
        className="max-w-full max-h-48 sm:max-h-64 lg:max-h-80 rounded-lg border border-zinc-700"
        preload="metadata"
        onError={() => setPlayError(true)}
      />
      <div className="flex items-center gap-2 text-[10px] text-zinc-600">
        <span className="truncate">{name || 'Video'}</span>
        {fileSize != null && <span>({(fileSize / 1048576).toFixed(1)} MB)</span>}
      </div>
    </div>
  )
}

function FileCard({ path, mime, name }: { path: string; mime?: string; name?: string }) {
  const [loading, setLoading] = useState(false)
  const previewCtx = useContext(PreviewContext)

  const handleClick = async () => {
    setLoading(true)
    try {
      let blobUrl = imageCache.get(path)
      let resolvedMime = mime || 'application/octet-stream'
      if (!blobUrl) {
        const res = await (window as any).myclawRequest?.('file.read', { path }) as { data?: string; mime?: string } | undefined
        if (!res?.data) return
        resolvedMime = res.mime || mime || 'application/octet-stream'
        const bin = atob(res.data)
        const bytes = new Uint8Array(bin.length)
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
        const blob = new Blob([bytes], { type: resolvedMime })
        blobUrl = URL.createObjectURL(blob)
        imageCache.set(path, blobUrl)
      }
      previewCtx.open({ src: blobUrl, mime: resolvedMime, name: name || 'file' })
    } finally { setLoading(false) }
  }

  return (
    <button
      onClick={handleClick}
      disabled={loading}
      className="flex items-center gap-2 px-3 py-2 rounded-lg bg-zinc-800/60 border border-zinc-700 hover:border-zinc-500 transition-colors text-left"
    >
      <span className="text-lg">📄</span>
      <div className="flex-1 min-w-0">
        <div className="text-xs text-zinc-300 truncate">{name || 'File'}</div>
        {mime && <div className="text-[10px] text-zinc-600">{mime}</div>}
      </div>
      {loading && <div className="h-3 w-3 border-2 border-zinc-600 border-t-zinc-300 rounded-full animate-spin" />}
    </button>
  )
}

function renderFileRef(file: { path: string; mime?: string; name?: string }, index: number) {
  const mime = file.mime || ''
  if (mime.startsWith('audio/')) return <AudioFileCard key={index} path={file.path} name={file.name} />
  if (mime.startsWith('video/')) return <VideoFileCard key={index} path={file.path} name={file.name} />
  return <FileCard key={index} path={file.path} mime={file.mime} name={file.name} />
}

// ── Memoized bubbles ─────────────────────────────────────────────────────

const UserBubble = memo(function UserBubble({ content, images, files }: {
  content: string
  images?: { path: string; mime?: string; name?: string }[]
  files?: { path: string; mime?: string; name?: string }[]
}) {
  return (
    <div className="flex justify-end gap-2.5 sm:gap-3.5">
      <div className="max-w-[85%] sm:max-w-[78%] lg:max-w-[72%] rounded-2xl rounded-tr-lg bg-zinc-800 px-3 sm:px-4 lg:px-5 py-2.5 sm:py-3 lg:py-3.5 text-sm text-zinc-100 whitespace-pre-wrap leading-relaxed">
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
        {content}
      </div>
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
      <div className="flex-1 min-w-0 rounded-2xl border border-zinc-800/80 bg-zinc-900/25 px-3 sm:px-4 lg:px-5 py-3 sm:py-4 space-y-3 shadow-sm hover:border-zinc-800 transition-colors">
        {blocks.map((block, i) => renderBlock(block, i, isGenerating))}
        {isGenerating && blocks.length === 0 && <GeneratingDots />}
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

  // Lightbox state
  const [lightboxOpen, setLightboxOpen] = useState(false)
  const [lightboxIndex, setLightboxIndex] = useState(0)

  // File preview state
  const [previewItem, setPreviewItem] = useState<PreviewItem | null>(null)

  const openLightbox = useCallback((blobUrl: string) => {
    const idx = slideIndex.get(blobUrl)
    if (idx !== undefined) {
      setLightboxIndex(idx)
      setLightboxOpen(true)
    }
  }, [])

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

  // Auto-scroll when near bottom — triggers on new messages AND content growth.
  // Track total content length of the last assistant message to detect streaming updates.
  const lastMsg = messages[messages.length - 1]
  const lastContentLen = lastMsg?.role === 'assistant'
    ? lastMsg.blocks.reduce((n, b) => n + (b.type === 'content' ? b.text.length : b.type === 'thinking' ? b.text.length : 0), 0)
    : 0

  useEffect(() => {
    if (isNearBottom) {
      virtualizer.scrollToIndex(messages.length - 1, { align: 'end' })
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messages.length, lastContentLen, isNearBottom, virtualizer])

  const scrollToBottom = () => {
    virtualizer.scrollToIndex(messages.length - 1, { align: 'end', behavior: 'smooth' })
  }

  if (messages.length === 0) {
    return <div ref={containerRef} className="flex-1 overflow-y-auto" />
  }

  const virtualItems = virtualizer.getVirtualItems()

  // Build slides array from the ordered registry for the lightbox
  const slides = slideRegistry.map((url) => ({ src: url }))

  return (
    <LightboxContext.Provider value={lightboxCtx.current}>
    <PreviewContext.Provider value={previewCtx.current}>
      <div className="flex-1 relative">
        <div ref={(el) => { scrollElementRef.current = el; if (containerRef) (containerRef as any).current = el }} className="absolute inset-0 overflow-y-auto">
          <div style={{ height: virtualizer.getTotalSize(), width: '100%', position: 'relative' }}>
            <div style={{ position: 'absolute', top: 0, left: 0, width: '100%', transform: `translateY(${virtualItems[0]?.start ?? 0}px)` }}>
              {virtualItems.map((vi) => {
                const msg = messages[vi.index]
                return (
                  <div key={vi.key} data-index={vi.index} ref={(el) => { if (el) virtualizer.measureElement(el) }} className="max-w-3xl lg:max-w-4xl 2xl:max-w-5xl mx-auto px-2 sm:px-4 lg:px-6 py-3 sm:py-4">
                    {msg.role === 'user' ? (
                      <UserBubble content={msg.content} images={'images' in msg ? (msg as any).images : undefined} files={'files' in msg ? (msg as any).files : undefined} />
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
          carousel={{ finite: slideRegistry.length <= 1 }}
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
  )
}
