import { useState, useEffect, createContext, useContext } from 'react'
import type { ChatMessage } from '../hooks/useWebSocket'

// ── Blob URL Cache ────────────────────────────────────────────────────────────

const BLOB_CACHE_LIMIT = 80
const imageCache = new Map<string, string>()

export function cacheBlobUrl(key: string, url: string) {
  const old = imageCache.get(key)
  if (old && old !== url) URL.revokeObjectURL(old)
  if (old) imageCache.delete(key)
  imageCache.set(key, url)
  while (imageCache.size > BLOB_CACHE_LIMIT) {
    const oldest = imageCache.entries().next().value as [string, string] | undefined
    if (!oldest) break
    imageCache.delete(oldest[0])
    URL.revokeObjectURL(oldest[1])
  }
}

export function releaseUnusedBlobUrls(messages: ChatMessage[], activeUrls: string[]) {
  const keepKeys = new Set<string>()
  messages.forEach((msg) => {
    if (msg.role !== 'user') return
    msg.images?.forEach((file) => keepKeys.add(file.path))
    msg.files?.forEach((file) => keepKeys.add(file.path))
  })
  const keepUrls = new Set(activeUrls)
  imageCache.forEach((url, key) => {
    if (!keepKeys.has(key) && !keepUrls.has(url)) {
      imageCache.delete(key)
      URL.revokeObjectURL(url)
    }
  })
}

export function base64ToBlobUrl(data: string, mime: string): string {
  const bin = atob(data)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  return URL.createObjectURL(new Blob([bytes], { type: mime }))
}

export function dataUrlToBlobUrl(dataUrl: string): string {
  const [header, b64] = dataUrl.split(',')
  const mime = header.match(/data:(.*?);/)?.[1] || 'application/octet-stream'
  return base64ToBlobUrl(b64, mime)
}

export function getImageCache() {
  return imageCache
}

// ── File Request Context ──────────────────────────────────────────────────────

interface FileRequestCtx {
  request: (method: string, params?: Record<string, unknown>, timeoutMs?: number) => Promise<unknown>
}

export const FileRequestContext = createContext<FileRequestCtx>({
  request: async () => { throw new Error('File request context is unavailable') }
})

// ── File Components ───────────────────────────────────────────────────────────

export function LazyImage({ path, mime, name }: { path: string; mime?: string; name?: string }) {
  const [src, setSrc] = useState<string | null>(() => imageCache.get(path) ?? null)
  const [error, setError] = useState(false)
  const { request } = useContext(FileRequestContext)

  useEffect(() => {
    if (imageCache.has(path)) {
      const cached = imageCache.get(path)!
      setSrc(cached)
      return
    }
    const fetchImage = async () => {
      try {
        const res = await request('file.read', { path }) as { data?: string; mime?: string } | undefined
        if (res?.data) {
          const mimeStr = res.mime || mime || 'image/png'
          const dataUrl = `data:${mimeStr};base64,${res.data}`
          const blobUrl = dataUrlToBlobUrl(dataUrl)
          cacheBlobUrl(path, blobUrl)
          setSrc(blobUrl)
        } else {
          setError(true)
        }
      } catch {
        setError(true)
      }
    }
    fetchImage()
  }, [path, mime, request])

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
      onClick={() => {
        // Lightbox handled by parent
      }}
    />
  )
}

export function AudioFileCard({ path, name }: { path: string; name?: string }) {
  const [src, setSrc] = useState<string | null>(() => imageCache.get(path) ?? null)
  const [error, setError] = useState(false)
  const { request } = useContext(FileRequestContext)

  useEffect(() => {
    if (imageCache.has(path)) { setSrc(imageCache.get(path)!); return }
    const fetchFile = async () => {
      try {
        const res = await request('file.read', { path }) as { data?: string; mime?: string } | undefined
        if (res?.data) {
          const mimeStr = res.mime || 'audio/mpeg'
          const blobUrl = base64ToBlobUrl(res.data, mimeStr)
          cacheBlobUrl(path, blobUrl)
          setSrc(blobUrl)
        } else { setError(true) }
      } catch { setError(true) }
    }
    fetchFile()
  }, [path, request])

  if (error) return <div className="text-xs text-zinc-600 italic">🎵 {name || 'Audio unavailable'}</div>
  if (!src) return <div className="w-48 h-10 rounded-lg bg-zinc-800 border border-zinc-700 animate-pulse" />

  return (
    <div className="flex items-center gap-2 px-3 py-2 rounded-lg bg-zinc-800/60 border border-zinc-700">
      <span className="text-xs text-zinc-400 truncate max-w-[120px]">{name || 'Audio'}</span>
      <audio controls src={src} className="h-8 flex-1 min-w-0" />
    </div>
  )
}

export function VideoFileCard({ path, name }: { path: string; name?: string }) {
  const [src, setSrc] = useState<string | null>(() => imageCache.get(path) ?? null)
  const [error, setError] = useState(false)
  const [playError, setPlayError] = useState(false)
  const [fileSize, setFileSize] = useState<number | null>(null)
  const [downloading, setDownloading] = useState(false)
  const { request } = useContext(FileRequestContext)

  useEffect(() => {
    if (imageCache.has(path)) { setSrc(imageCache.get(path)!); return }
    const fetchFile = async () => {
      try {
        const res = await request('file.read', { path }) as { data?: string; mime?: string; size?: number } | undefined
        if (res?.data) {
          const mimeStr = res.mime || 'video/mp4'
          const blobUrl = base64ToBlobUrl(res.data, mimeStr)
          cacheBlobUrl(path, blobUrl)
          setSrc(blobUrl)
          setFileSize(res.size ?? null)
        } else { setError(true) }
      } catch { setError(true) }
    }
    fetchFile()
  }, [path, request])

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

export function FileCard({ path, mime, name }: { path: string; mime?: string; name?: string }) {
  const [loading, setLoading] = useState(false)
  const { request } = useContext(FileRequestContext)

  const handleClick = async () => {
    setLoading(true)
    try {
      let blobUrl = imageCache.get(path)
      let resolvedMime = mime || 'application/octet-stream'
      if (!blobUrl) {
        const res = await request('file.read', { path }) as { data?: string; mime?: string } | undefined
        if (!res?.data) return
        resolvedMime = res.mime || mime || 'application/octet-stream'
        blobUrl = base64ToBlobUrl(res.data, resolvedMime)
        cacheBlobUrl(path, blobUrl)
      }
      // Open in new tab or trigger preview
      window.open(blobUrl, '_blank')
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

export function renderFileRef(file: { path: string; mime?: string; name?: string }, index: number) {
  const mime = file.mime || ''
  if (mime.startsWith('audio/')) return <AudioFileCard key={index} path={file.path} name={file.name} />
  if (mime.startsWith('video/')) return <VideoFileCard key={index} path={file.path} name={file.name} />
  return <FileCard key={index} path={file.path} mime={file.mime} name={file.name} />
}
