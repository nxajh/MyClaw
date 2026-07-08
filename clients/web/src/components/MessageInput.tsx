import { useState, useRef, useEffect, useCallback, type KeyboardEvent } from 'react'
import { ArrowUp, Square, Paperclip, X, FileText, Image as ImageIcon, Film, Music, File, Eye, EyeOff } from 'lucide-react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import type { SendOptions, FileAttachment } from '../hooks/useWebSocket'

interface Props {
  onSend: (content: string, opts?: SendOptions) => void
  onCancel: () => void
  disabled: boolean
  isGenerating: boolean
}

interface Command { name: string; description: string }
interface PickedImage { name: string; dataUrl: string }
interface PickedText { name: string; content: string }
interface PickedBinary { name: string; mime: string; dataUrl: string }

const IMAGE_MAX = 10 * 1024 * 1024
const TEXT_MAX = 256 * 1024
const BINARY_MAX = 150 * 1024 * 1024
// After base64 encoding, a file grows ~33%. WS server limit is 200MiB.
// Keep raw file under 140MiB so base64 payload stays under ~186MiB.
const BINARY_SEND_MAX = 140 * 1024 * 1024
const ACCEPT = '*/*'
const INPUT_HISTORY_KEY = 'myclaw_input_history'

export default function MessageInput({ onSend, onCancel, disabled, isGenerating }: Props) {
  const { status, request, clearInputToken } = useWebSocketContext()
  const [text, setText] = useState('')
  const [images, setImages] = useState<PickedImage[]>([])
  const [texts, setTexts] = useState<PickedText[]>([])
  const [binaries, setBinaries] = useState<PickedBinary[]>([])
  const [note, setNote] = useState<string | null>(null)
  const [previewMode, setPreviewMode] = useState(false)
  const [history, setHistory] = useState<string[]>(() => {
    try { return JSON.parse(localStorage.getItem(INPUT_HISTORY_KEY) || '[]') } catch { return [] }
  })
  const [historyIdx, setHistoryIdx] = useState<number | null>(null)
  const draftBeforeHistory = useRef('')
  const noteTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const [isDragging, setIsDragging] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileRef = useRef<HTMLInputElement>(null)

  // Auto-clear note after 5 seconds
  const clearNote = useCallback(() => {
    if (noteTimerRef.current) clearTimeout(noteTimerRef.current)
    noteTimerRef.current = setTimeout(() => setNote(null), 5000)
  }, [])

  // Cleanup timer on unmount
  useEffect(() => () => { if (noteTimerRef.current) clearTimeout(noteTimerRef.current) }, [])

  // Clear input when session switches
  const prevClearTokenRef = useRef(clearInputToken)
  useEffect(() => {
    if (clearInputToken !== prevClearTokenRef.current) {
      prevClearTokenRef.current = clearInputToken
      setText('')
      setImages([])
      setTexts([])
      setBinaries([])
      setNote(null)
      setPreviewMode(false)
      if (textareaRef.current) textareaRef.current.style.height = 'auto'
    }
  }, [clearInputToken])

  // Focus input on '/' when not in an input field
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (disabled) return
      const tag = (e.target as HTMLElement)?.tagName
      if (tag === 'INPUT' || tag === 'TEXTAREA' || (e.target as HTMLElement)?.isContentEditable) return
      if (e.key === '/' && !e.metaKey && !e.ctrlKey) {
        e.preventDefault()
        textareaRef.current?.focus()
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [disabled])

  const handleDragOver = (e: React.DragEvent) => {
    e.preventDefault()
    if (!disabled) setIsDragging(true)
  }

  const handleDragLeave = () => {
    setIsDragging(false)
  }

  const handleDrop = (e: React.DragEvent) => {
    e.preventDefault()
    setIsDragging(false)
    if (disabled) return
    if (e.dataTransfer.files && e.dataTransfer.files.length > 0) {
      handleFiles(e.dataTransfer.files)
    }
  }

  // ── Slash-command autocomplete ────────────────────────────────────────────
  const [commands, setCommands] = useState<Command[]>([])
  const [cmdSel, setCmdSel] = useState(0)
  const loadedCmds = useRef(false)

  useEffect(() => {
    if (status !== 'connected' || loadedCmds.current) return
    loadedCmds.current = true
    request('commands.list')
      .then((r) => setCommands((r as Command[]) || []))
      .catch(() => { loadedCmds.current = false })
  }, [status, request])

  const slashQuery = text.startsWith('/') && !text.includes(' ') && !text.includes('\n')
    ? text.slice(1).toLowerCase()
    : null
  const matches = slashQuery !== null
    ? commands.filter((c) => c.name.toLowerCase().startsWith(slashQuery))
    : []
  const showCmds = matches.length > 0

  useEffect(() => { setCmdSel(0) }, [slashQuery])

  const acceptCommand = useCallback((cmd: Command) => {
    setText(`/${cmd.name} `)
    textareaRef.current?.focus()
  }, [])

  // ── File handling ─────────────────────────────────────────────────────────
  const isTextMime = (t: string) => t.startsWith('text/') || ['application/json', 'application/xml', 'application/javascript', 'application/x-sh'].includes(t)
  const isTextExt = (name: string) => /\.(txt|md|json|js|ts|tsx|jsx|py|rs|go|java|c|cpp|h|hpp|css|html|yml|yaml|toml|sh|log|csv|xml|sql|rb|php|swift|kt|scala|lua|r|jl|m|mm|vue|svelte|astro|mdx|conf|cfg|ini|env|dockerfile|makefile)$/i.test(name)

  const handleFiles = useCallback((files: FileList | null) => {
    if (!files) return
    setNote(null)
    let hasNote = false
    Array.from(files).forEach((file) => {
      const isImage = file.type.startsWith('image/')
      const isText = isTextMime(file.type) || isTextExt(file.name)
      if (isImage) {
        if (file.size > IMAGE_MAX) { setNote(`${file.name} skipped (image > 10MB)`); hasNote = true; return }
        const reader = new FileReader()
        reader.onload = () => setImages((p) => [...p, { name: file.name, dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      } else if (isText) {
        if (file.size > TEXT_MAX) { setNote(`${file.name} skipped (file > 256KB)`); hasNote = true; return }
        const reader = new FileReader()
        reader.onload = () => setTexts((p) => [...p, { name: file.name, content: reader.result as string }])
        reader.readAsText(file)
      } else {
        if (file.size > BINARY_MAX) { setNote(`${file.name} skipped (file > ${Math.round(BINARY_MAX / 1024 / 1024)}MB)`); hasNote = true; return }
        const reader = new FileReader()
        reader.onload = () => setBinaries((p) => [...p, { name: file.name, mime: file.type || 'application/octet-stream', dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      }
    })
    if (hasNote) clearNote()
  }, [clearNote])

  // ── Send ──────────────────────────────────────────────────────────────────
  const handleSend = () => {
    const trimmed = text.trim()
    if (disabled) return
    if (!trimmed && images.length === 0 && texts.length === 0 && binaries.length === 0) return
    // Check total base64 payload size before sending
    const totalRaw = binaries.reduce((n, b) => n + b.dataUrl.length, 0)
    if (totalRaw > BINARY_SEND_MAX * 4 / 3) {
      setNote(`Total attachment size too large (>${Math.round(BINARY_SEND_MAX / 1024 / 1024)}MB raw). Try smaller files.`)
      return
    }
    const fileAttachments: FileAttachment[] = binaries.map((b) => {
      const b64 = b.dataUrl.split(',')[1] || b.dataUrl
      return { data: b64, mime_type: b.mime, file_name: b.name }
    })
    setHistory((prev) => {
      const next = [trimmed, ...prev.filter(item => item !== trimmed)].slice(0, 50)
      localStorage.setItem(INPUT_HISTORY_KEY, JSON.stringify(next))
      return next
    })
    setHistoryIdx(null)
    draftBeforeHistory.current = ''
    onSend(trimmed, {
      images: images.map((i) => i.dataUrl),
      attachments: texts.map((t) => ({ name: t.name, content: t.content })),
      files: fileAttachments,
    })
    setText('')
    setImages([])
    setTexts([])
    setBinaries([])
    setNote(null)
    if (textareaRef.current) textareaRef.current.style.height = 'auto'
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (showCmds) {
      if (e.key === 'ArrowDown') { e.preventDefault(); setCmdSel((i) => Math.min(i + 1, matches.length - 1)); return }
      if (e.key === 'ArrowUp') { e.preventDefault(); setCmdSel((i) => Math.max(i - 1, 0)); return }
      if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
        e.preventDefault(); acceptCommand(matches[cmdSel]); return
      }
      if (e.key === 'Escape') { e.preventDefault(); setText(''); return }
    }
    if (e.key === 'ArrowUp' && !e.shiftKey && !showCmds && textareaRef.current?.selectionStart === 0 && textareaRef.current.selectionEnd === 0 && history.length > 0) {
      e.preventDefault()
      const nextIdx = historyIdx === null ? 0 : Math.min(historyIdx + 1, history.length - 1)
      if (historyIdx === null) draftBeforeHistory.current = text
      setHistoryIdx(nextIdx)
      setText(history[nextIdx])
      requestAnimationFrame(() => { const el = textareaRef.current; if (el) { el.selectionStart = el.selectionEnd = el.value.length; handleInput() } })
      return
    }
    if (e.key === 'ArrowDown' && !e.shiftKey && historyIdx !== null) {
      e.preventDefault()
      const nextIdx = historyIdx - 1
      if (nextIdx < 0) {
        setHistoryIdx(null)
        setText(draftBeforeHistory.current)
      } else {
        setHistoryIdx(nextIdx)
        setText(history[nextIdx])
      }
      requestAnimationFrame(() => { const el = textareaRef.current; if (el) { el.selectionStart = el.selectionEnd = el.value.length; handleInput() } })
      return
    }
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault()
      handleSend()
      return
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      handleSend()
    }
  }

  const handleInput = () => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = Math.min(el.scrollHeight, 200) + 'px'
  }

  const hasAttachments = images.length > 0 || texts.length > 0 || binaries.length > 0
  const canSend = !disabled && (text.trim().length > 0 || hasAttachments)

  const fileIcon = (_name: string, mime: string) => {
    if (mime.startsWith('video/')) return <Film size={12} className="text-purple-400 shrink-0" />
    if (mime.startsWith('audio/')) return <Music size={12} className="text-green-400 shrink-0" />
    return <File size={12} className="text-blue-400 shrink-0" />
  }

  return (
    <div className="px-2 sm:px-4 pb-3 sm:pb-5 pt-2 sm:pt-3 shrink-0 safe-area-bottom">
      <div className="relative">
        {/* Slash command dropdown */}
        {showCmds && (
          <div className="absolute bottom-full mb-2 left-0 right-0 max-h-64 overflow-y-auto rounded-xl border border-zinc-800 bg-zinc-900 shadow-2xl py-1 z-20">
            {matches.map((c, i) => (
              <button
                key={c.name}
                onMouseEnter={() => setCmdSel(i)}
                onClick={() => acceptCommand(c)}
                className={`w-full flex items-baseline gap-2 px-3 py-2 text-left transition-colors ${
                  i === cmdSel ? 'bg-zinc-800' : 'hover:bg-zinc-800/60'
                }`}
              >
                <span className="font-mono text-sm text-blue-400 shrink-0">/{c.name}</span>
                <span className="text-xs text-zinc-500 truncate">{c.description}</span>
              </button>
            ))}
          </div>
        )}

        {/* Input box */}
        <div
          onDragOver={handleDragOver}
          onDragLeave={handleDragLeave}
          onDrop={handleDrop}
          className={`relative rounded-2xl border shadow-xl transition-all ${
            isDragging
              ? 'border-blue-500 bg-zinc-900/90 scale-[1.01] ring-1 ring-blue-500/30'
              : 'border-zinc-700/60 bg-zinc-900 focus-within:border-zinc-500 focus-within:ring-1 focus-within:ring-zinc-500/30'
          }`}
        >
          {isDragging && (
            <div className="absolute inset-0 z-30 rounded-2xl bg-zinc-950/75 backdrop-blur-[2px] flex items-center justify-center pointer-events-none">
              <span className="text-sm font-medium text-blue-400 flex items-center gap-2">
                <Paperclip size={16} className="animate-bounce" />
                Drop files here to attach
              </span>
            </div>
          )}

          {/* Attachment chips */}
          {hasAttachments && (
            <div className="flex flex-wrap gap-2 px-4 pt-3">
              {images.map((img, i) => (
                <div key={`i${i}`} className="relative group">
                  <img src={img.dataUrl} alt={img.name} className="h-16 w-16 object-cover rounded-lg border border-zinc-700" />
                  <button
                    onClick={() => setImages((p) => p.filter((_, j) => j !== i))}
                    className="absolute -top-1.5 -right-1.5 h-5 w-5 rounded-full bg-zinc-800 border border-zinc-600 flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity"
                  >
                    <X size={11} className="text-zinc-300" />
                  </button>
                </div>
              ))}
              {texts.map((t, i) => (
                <div key={`t${i}`} className="flex items-center gap-1.5 h-8 px-2.5 rounded-lg bg-zinc-800 border border-zinc-700 text-xs text-zinc-300">
                  <FileText size={12} className="text-blue-400 shrink-0" />
                  <span className="max-w-[140px] truncate">{t.name}</span>
                  <button onClick={() => setTexts((p) => p.filter((_, j) => j !== i))} className="ml-1 text-zinc-500 hover:text-zinc-300">
                    <X size={11} />
                  </button>
                </div>
              ))}
              {binaries.map((b, i) => (
                <div key={`b${i}`} className="flex items-center gap-1.5 h-8 px-2.5 rounded-lg bg-zinc-800 border border-zinc-700 text-xs text-zinc-300">
                  {fileIcon(b.name, b.mime)}
                  <span className="max-w-[140px] truncate">{b.name}</span>
                  <button onClick={() => setBinaries((p) => p.filter((_, j) => j !== i))} className="ml-1 text-zinc-500 hover:text-zinc-300">
                    <X size={11} />
                  </button>
                </div>
              ))}
            </div>
          )}

          {/* Textarea or Preview */}
          {previewMode && text.trim() ? (
            <div className="px-3 sm:px-4 pt-3 sm:pt-4 pb-14 min-h-[120px] max-h-[200px] overflow-y-auto prose prose-invert prose-sm max-w-none prose-p:my-1 prose-li:my-0 prose-code:text-zinc-400 prose-code:bg-zinc-800/60 prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none prose-a:text-blue-400 prose-table:text-xs">
              <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
            </div>
          ) : (
          <textarea
            ref={textareaRef}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={handleKeyDown}
            onInput={handleInput}
            onPaste={(e) => {
              if (disabled) return
              if (e.clipboardData.files && e.clipboardData.files.length > 0) {
                e.preventDefault()
                handleFiles(e.clipboardData.files)
              }
            }}
            placeholder={disabled ? 'Connecting…' : 'Message MyClaw…  (/ for commands)'}
            disabled={disabled && !isGenerating}
            rows={1}
            className="w-full resize-none bg-transparent px-3 sm:px-4 pt-3 sm:pt-4 pb-14 text-sm sm:text-base text-zinc-100 placeholder-zinc-600 outline-none disabled:opacity-50 leading-relaxed"
            style={{ maxHeight: 200 }}
          />
          )}

          {/* Toolbar inside box */}
          <div className="absolute bottom-3 left-3 right-3 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <button
                onClick={() => fileRef.current?.click()}
                disabled={disabled}
                className="flex items-center justify-center h-8 w-8 rounded-lg text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800 disabled:opacity-30 transition-colors"
                title="Attach files or images"
              >
                <Paperclip size={15} />
              </button>
              <input
                ref={fileRef}
                type="file"
                multiple
                accept={ACCEPT}
                className="hidden"
                onChange={(e) => { handleFiles(e.target.files); e.target.value = '' }}
              />
              <button
                onClick={() => setPreviewMode((v) => !v)}
                disabled={disabled || !text.trim()}
                className={`flex items-center justify-center h-8 w-8 rounded-lg transition-colors disabled:opacity-30 ${previewMode ? 'text-blue-400 bg-zinc-800' : 'text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800'}`}
                title={previewMode ? 'Edit mode' : 'Preview Markdown'}
              >
                {previewMode ? <EyeOff size={15} /> : <Eye size={15} />}
              </button>
              <span className="text-xs text-zinc-600 select-none">
                {note ? <span className="text-amber-500">{note}</span>
                  : isGenerating ? 'Generating…'
                  : <span className="hidden sm:flex items-center gap-1"><ImageIcon size={11} /> ↑ history · Shift+Enter newline · Ctrl+Enter send</span>}
              </span>
            </div>
            {isGenerating ? (
              <button
                onClick={onCancel}
                className="flex items-center justify-center h-8 w-8 rounded-lg bg-zinc-700 hover:bg-zinc-600 transition-colors"
                title="Stop generating"
              >
                <Square size={13} className="text-zinc-300" />
              </button>
            ) : (
              <button
                onClick={handleSend}
                disabled={!canSend}
                className="flex items-center justify-center h-8 w-8 rounded-lg bg-zinc-100 hover:bg-white disabled:opacity-25 disabled:cursor-not-allowed transition-colors"
                title="Send (Enter / Ctrl+Enter)"
              >
                <ArrowUp size={15} className="text-zinc-900" />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
