import { useState, useEffect } from 'react'

export interface PreviewItem {
  src: string
  mime: string
  name: string
}

export function FilePreviewModal({ item, onClose }: { item: PreviewItem; onClose: () => void }) {
  const [zoom, setZoom] = useState(1)
  const [rotation, setRotation] = useState(0)
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
  const handleResetZoom = () => { setZoom(1); setRotation(0) }
  const handleRotate = () => setRotation((r) => (r + 90) % 360)

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
              {isImage && (
                <button onClick={(e) => { e.stopPropagation(); handleRotate() }} className="p-1.5 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200 transition-colors" title="Rotate">
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M21 12a9 9 0 1 1-3-6.7"/><path d="M21 3v6h-6"/></svg>
                </button>
              )}
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
          <img src={item.src} alt={item.name} style={{ transform: `scale(${zoom}) rotate(${rotation}deg)` }} className="max-w-full max-h-full object-contain transition-transform" />
        )}
        {isVideo && (
          <video controls autoPlay src={item.src} className="max-w-full max-h-full rounded-lg" />
        )}
        {isAudio && (
          <div className="flex flex-col items-center gap-4">
            <span className="text-6xl">🎵</span>
            <audio controls autoPlay src={item.src} className="w-80" />
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
