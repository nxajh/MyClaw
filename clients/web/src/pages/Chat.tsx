import { useRef, useEffect } from 'react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import MessageList from '../components/MessageList'
import MessageInput from '../components/MessageInput'

export default function Chat() {
  const { status, messages, isGenerating, sendMessage, cancel } = useWebSocketContext()
  const bottomRef = useRef<HTMLDivElement>(null)

  // Auto-scroll to bottom on new messages
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  return (
    <>
      {/* Header */}
      <header className="border-b border-zinc-700/50 px-6 py-3 flex items-center justify-between shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300">Chat</h2>
        <span
          className={`text-xs ${status === 'connected' ? 'text-emerald-400' : status === 'connecting' ? 'text-amber-400' : 'text-red-400'}`}
        >
          {status === 'connected' ? '● Online' : status === 'connecting' ? '● Connecting' : '● Offline'}
        </span>
      </header>

      {/* Messages */}
      <MessageList messages={messages} />
      <div ref={bottomRef} />

      {/* Input */}
      <MessageInput
        onSend={sendMessage}
        onCancel={cancel}
        disabled={status !== 'connected'}
        isGenerating={isGenerating}
      />
    </>
  )
}
