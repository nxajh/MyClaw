import { useRef, useEffect } from 'react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import MessageList from '../components/MessageList'
import MessageInput from '../components/MessageInput'

export default function Chat() {
  const { status, messages, isGenerating, sendMessage, cancel } = useWebSocketContext()
  const containerRef = useRef<HTMLDivElement>(null)

  // Scroll to bottom whenever messages update.
  useEffect(() => {
    const el = containerRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [messages])

  return (
    <div className="flex flex-col h-full">
      <MessageList messages={messages} containerRef={containerRef} />
      <MessageInput
        onSend={sendMessage}
        onCancel={cancel}
        disabled={status !== 'connected'}
        isGenerating={isGenerating}
      />
    </div>
  )
}
