import { useState, type KeyboardEvent } from 'react'
import { Send, Square } from 'lucide-react'

interface Props {
  onSend: (content: string) => void
  onCancel: () => void
  disabled: boolean
  isGenerating: boolean
}

export default function MessageInput({ onSend, onCancel, disabled, isGenerating }: Props) {
  const [text, setText] = useState('')

  const handleSend = () => {
    const trimmed = text.trim()
    if (!trimmed || disabled) return
    onSend(trimmed)
    setText('')
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      handleSend()
    }
  }

  return (
    <div className="border-t border-zinc-700/50 bg-zinc-900 px-4 py-3">
      <div className="flex items-end gap-2 max-w-4xl mx-auto">
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={disabled ? 'Connecting…' : 'Type a message… (Enter to send)'}
          disabled={disabled}
          rows={1}
          className="flex-1 resize-none rounded-xl border border-zinc-700/50 bg-zinc-800 px-4 py-3 text-sm text-zinc-100 placeholder-zinc-500 outline-none focus:border-zinc-600 focus:ring-1 focus:ring-zinc-600 disabled:opacity-50 transition"
          style={{ maxHeight: '200px' }}
          onInput={(e) => {
            const el = e.currentTarget
            el.style.height = 'auto'
            el.style.height = Math.min(el.scrollHeight, 200) + 'px'
          }}
        />
        {isGenerating ? (
          <button
            onClick={onCancel}
            className="flex items-center gap-1.5 rounded-xl bg-red-600/80 hover:bg-red-600 px-4 py-3 text-sm font-medium transition"
            title="Cancel generation"
          >
            <Square size={14} />
            Stop
          </button>
        ) : (
          <button
            onClick={handleSend}
            disabled={disabled || !text.trim()}
            className="flex items-center gap-1.5 rounded-xl bg-blue-600 hover:bg-blue-500 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-3 text-sm font-medium transition"
            title="Send message"
          >
            <Send size={14} />
            Send
          </button>
        )}
      </div>
    </div>
  )
}
