import { useRef, useCallback, useMemo } from 'react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from '../components/Toast'
import MessageList from '../components/MessageList'
import MessageInput from '../components/MessageInput'
import ChatHeader from '../components/ChatHeader'

import type { SendOptions } from '../hooks/useWebSocket'

const ALL_EXAMPLES = [
  { icon: '💡', text: 'Explain async/await in Rust' },
  { icon: '🔧', text: 'Write a Python script to rename files' },
  { icon: '📚', text: 'Summarize the concept of closures' },
  { icon: '🏗️', text: 'Design a REST API for a todo app' },
  { icon: '🐛', text: 'Debug this error: connection refused on port 8080' },
  { icon: '📊', text: 'Analyze this CSV and find trends' },
  { icon: '🧪', text: 'Write unit tests for this function' },
  { icon: '🔍', text: 'Find performance bottlenecks in this code' },
  { icon: '🌐', text: 'Explain how DNS resolution works' },
  { icon: '📝', text: 'Refactor this code to be more readable' },
  { icon: '🛡️', text: 'Review this code for security issues' },
  { icon: '📦', text: 'Create a Dockerfile for a Node.js app' },
]

export default function Chat() {
  const { status, messages, isGenerating, sendMessage, cancel, setMessages, historyLoading } = useWebSocketContext()
  const { toast } = useToast()
  const containerRef = useRef<HTMLDivElement>(null)

  const handleRetry = useCallback(
    (userContent: string) => {
      if (isGenerating || status !== 'connected') return
      // Remove the last assistant message so the new response replaces it
      setMessages((prev) => {
        const last = prev[prev.length - 1]
        if (last && last.role === 'assistant') return prev.slice(0, -1)
        return prev
      })
      sendMessage(userContent)
    },
    [isGenerating, status, sendMessage, setMessages],
  )

  const handleSend = useCallback(
    (text: string, opts?: SendOptions) => {
      if (status !== 'connected') {
        toast('Not connected — message not sent', 'error')
        return
      }
      sendMessage(text, opts)
    },
    [status, sendMessage, toast],
  )

  const showEmpty = messages.length === 0 && status === 'connected' && !isGenerating && !historyLoading

  const suggestions = useMemo(() => {
    const shuffled = [...ALL_EXAMPLES].sort(() => Math.random() - 0.5)
    return shuffled.slice(0, 4)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [showEmpty])

  return (
    <div className="flex flex-col h-full">
      <ChatHeader />
      {historyLoading && messages.length === 0 ? (
        <div className="flex-1 overflow-y-auto">
          <div className="px-3 sm:px-8 py-8 space-y-6 max-w-3xl mx-auto w-full">
            {[...Array(3)].map((_, i) => (
              <div key={i} className="space-y-3">
                <div className="skeleton-line w-3/4" />
                <div className="skeleton-line w-full" />
                <div className="skeleton-line w-5/6" />
                <div className="skeleton-line w-1/2" />
              </div>
            ))}
          </div>
        </div>
      ) : showEmpty ? (
        <div className="flex-1 overflow-y-auto">
          <div className="px-3 sm:px-8 py-8 flex flex-col items-center justify-center min-h-full page-enter">
            <div className="w-full max-w-3xl">
              <div className="text-center mb-8">
                <div className="text-4xl mb-3">🐾</div>
                <h2 className="text-lg font-semibold tracking-tight text-zinc-200 mb-1">How can I help you today?</h2>
                <p className="text-sm text-zinc-500">Ask anything — coding, writing, analysis, and more.</p>
              </div>
              <div className="grid grid-cols-1 sm:grid-cols-2 gap-2 w-full">
                {suggestions.map((ex) => (
                  <button
                    key={ex.text}
                    onClick={() => handleSend(ex.text)}
                    className="flex items-center gap-3 text-left rounded-xl border border-zinc-800 bg-zinc-900/40 px-4 py-3 hover:bg-zinc-900 hover:border-zinc-700 hover:-translate-y-0.5 hover:shadow-md active:scale-[0.99] transition-all text-sm text-zinc-300"
                  >
                    <span className="text-lg shrink-0">{ex.icon}</span>
                    <span>{ex.text}</span>
                  </button>
                ))}
              </div>
            </div>
          </div>
        </div>
      ) : (
        <MessageList messages={messages} containerRef={containerRef} onRetry={handleRetry} />
      )}
      <MessageInput
        onSend={handleSend}
        onCancel={cancel}
        disabled={status !== 'connected'}
        isGenerating={isGenerating}
      />
    </div>
  )
}
