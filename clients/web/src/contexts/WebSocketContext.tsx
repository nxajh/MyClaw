import { createContext, useContext, useCallback, useEffect, useRef, useState, type ReactNode, type Dispatch, type SetStateAction } from 'react'
import { useWebSocket } from '../hooks/useWebSocket'
import { useApi } from '../lib/api'
import type { ChatMessage, ConnectionStatus, SendOptions, LastCloseInfo } from '../hooks/useWebSocket'

interface WebSocketContextValue {
  status: ConnectionStatus
  lastCloseInfo: LastCloseInfo | null
  messages: ChatMessage[]
  isGenerating: boolean
  authFailed: boolean
  authValidating: boolean
  activeSessionId: string | null
  setActiveSessionId: (id: string | null) => void
  submitToken: (token: string, clientId: string) => void
  sendMessage: (content: string, opts?: SendOptions) => void
  cancel: () => void
  reconnectNow: () => void
  sendRaw: (obj: Record<string, unknown>) => boolean
  setMessages: Dispatch<SetStateAction<ChatMessage[]>>
  addMessageListener: (fn: (data: Record<string, unknown>) => void) => () => void
  request: (method: string, params?: Record<string, unknown>, timeoutMs?: number) => Promise<unknown>
  reloadHistory: () => Promise<void>
  clearInputToken: number
  triggerClearInput: () => void
  historyLoading: boolean
}

const WebSocketContext = createContext<WebSocketContextValue | null>(null)

export function WebSocketProvider({ children }: { children: ReactNode }) {
  const ws = useWebSocket()
  const { request } = useApi(ws.sendRaw, ws.addMessageListener)
  const { setMessages, status } = ws
  const [historyLoading, setHistoryLoading] = useState(false)

  const reloadHistory = useCallback(async () => {
    setHistoryLoading(true)
    try {
      const result = await request('sessions.history')
      setMessages((result as ChatMessage[] | null) ?? [])
    } catch {
      /* leave existing messages untouched on failure */
    } finally {
      setHistoryLoading(false)
    }
  }, [request, setMessages])

  // Auto-load history right after authentication succeeds.
  // This runs at the context level so it works regardless of which
  // page/component is currently mounted (e.g. login overlay hiding Chat).
  const prevStatusRef = useRef<ConnectionStatus>(status)
  useEffect(() => {
    if (prevStatusRef.current !== 'connected' && status === 'connected') {
      reloadHistory()
    }
    prevStatusRef.current = status
  }, [status, reloadHistory])

  return (
    <WebSocketContext.Provider value={{ ...ws, request, reloadHistory, historyLoading }}>
      {children}
    </WebSocketContext.Provider>
  )
}

export function useWebSocketContext(): WebSocketContextValue {
  const ctx = useContext(WebSocketContext)
  if (!ctx) throw new Error('useWebSocketContext must be used inside WebSocketProvider')
  return ctx
}
