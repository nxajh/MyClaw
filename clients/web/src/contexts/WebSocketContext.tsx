import { createContext, useContext, useCallback, useEffect, type ReactNode, type Dispatch, type SetStateAction } from 'react'
import { useWebSocket } from '../hooks/useWebSocket'
import { useApi } from '../lib/api'
import type { ChatMessage, ConnectionStatus, SendOptions } from '../hooks/useWebSocket'

interface WebSocketContextValue {
  status: ConnectionStatus
  messages: ChatMessage[]
  isGenerating: boolean
  authFailed: boolean
  authValidating: boolean
  activeSessionId: string | null
  setActiveSessionId: (id: string | null) => void
  submitToken: (token: string, clientId?: string) => void
  sendMessage: (content: string, opts?: SendOptions) => void
  cancel: () => void
  sendRaw: (obj: Record<string, unknown>) => void
  setMessages: Dispatch<SetStateAction<ChatMessage[]>>
  addMessageListener: (fn: (data: Record<string, unknown>) => void) => () => void
  request: (method: string, params?: Record<string, unknown>, timeoutMs?: number) => Promise<unknown>
  reloadHistory: () => Promise<void>
}

const WebSocketContext = createContext<WebSocketContextValue | null>(null)

export function WebSocketProvider({ children }: { children: ReactNode }) {
  const ws = useWebSocket()
  const { request } = useApi(ws.sendRaw, ws.addMessageListener)
  const { setMessages } = ws

  // Expose request globally for components that can't use context (e.g. LazyImage in memo'd UserBubble)
  useEffect(() => { (window as any).myclawRequest = request }, [request])

  const reloadHistory = useCallback(async () => {
    try {
      const result = await request('sessions.history')
      setMessages((result as ChatMessage[] | null) ?? [])
    } catch {
      /* leave existing messages untouched on failure */
    }
  }, [request, setMessages])

  return (
    <WebSocketContext.Provider value={{ ...ws, request, reloadHistory }}>
      {children}
    </WebSocketContext.Provider>
  )
}

export function useWebSocketContext(): WebSocketContextValue {
  const ctx = useContext(WebSocketContext)
  if (!ctx) throw new Error('useWebSocketContext must be used inside WebSocketProvider')
  return ctx
}
