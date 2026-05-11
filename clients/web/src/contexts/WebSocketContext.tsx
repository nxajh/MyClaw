import { createContext, useContext, type ReactNode, type Dispatch, type SetStateAction } from 'react'
import { useWebSocket } from '../hooks/useWebSocket'
import type { ChatMessage, ConnectionStatus } from '../hooks/useWebSocket'

interface WebSocketContextValue {
  status: ConnectionStatus
  messages: ChatMessage[]
  isGenerating: boolean
  sendMessage: (content: string) => void
  cancel: () => void
  sendRaw: (obj: Record<string, unknown>) => void
  setMessages: Dispatch<SetStateAction<ChatMessage[]>>
  addMessageListener: (fn: (data: Record<string, unknown>) => void) => () => void
}

const WebSocketContext = createContext<WebSocketContextValue | null>(null)

export function WebSocketProvider({ children }: { children: ReactNode }) {
  const ws = useWebSocket()
  return <WebSocketContext.Provider value={ws}>{children}</WebSocketContext.Provider>
}

export function useWebSocketContext(): WebSocketContextValue {
  const ctx = useContext(WebSocketContext)
  if (!ctx) throw new Error('useWebSocketContext must be used inside WebSocketProvider')
  return ctx
}
