import type { MessageBlock, ChatMessage, ConnectionStatus, LastCloseInfo } from './useWebSocket'

// ── Message processing utilities ───────────────────────────────────────────

/**
 * Finalize the current assistant message (mark as done, stop generating).
 */
export function finalizeAssistantMessage(
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>,
  setIsGenerating: (v: boolean) => void,
  currentAssistantId: React.MutableRefObject<string | null>,
) {
  setMessages((prev) => {
    const last = prev[prev.length - 1]
    if (last && last.role === 'assistant' && !last.done) {
      return [...prev.slice(0, -1), { ...last, done: true }]
    }
    return prev
  })
  setIsGenerating(false)
  currentAssistantId.current = null
}

/**
 * Handle auth_ok message from server.
 */
export function handleAuthOk(
  authPending: React.MutableRefObject<boolean>,
  setAuthFailed: (v: boolean) => void,
  setAuthValidating: (v: boolean) => void,
  setStatus: (v: ConnectionStatus) => void,
  setLastCloseInfo: (v: LastCloseInfo | null) => void,
  reconnectAttempts: React.MutableRefObject<number>,
) {
  authPending.current = false
  setAuthFailed(false)
  setAuthValidating(false)
  setStatus('connected')
  setLastCloseInfo(null)
  reconnectAttempts.current = 0
}

/**
 * Handle tool_call message from server.
 */
export function handleToolCall(
  data: Record<string, unknown>,
  uid: () => string,
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>,
  currentAssistantId: React.MutableRefObject<string | null>,
) {
  const callId = (data.id as string) || uid()
  const name = (data.name as string) || 'unknown'
  const args = (data.args as Record<string, unknown>) || {}
  setMessages((prev) => {
    const last = prev[prev.length - 1]
    if (last && last.role === 'assistant' && !last.done) {
      const blocks: MessageBlock[] = [...last.blocks, { type: 'tool_call', id: callId, name, args, startedAt: Date.now() }]
      return [...prev.slice(0, -1), { ...last, blocks }]
    }
    const id = uid()
    currentAssistantId.current = id
    return [...prev, { role: 'assistant', blocks: [{ type: 'tool_call', id: callId, name, args, startedAt: Date.now() }], id, done: false }]
  })
}

/**
 * Handle tool_result message from server.
 */
export function handleToolResult(
  data: Record<string, unknown>,
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>,
) {
  const callId = (data.id as string) || ''
  const name = (data.name as string) || 'unknown'
  const output = (data.output as string) || ''
  const isError = !!(data.error as boolean)
  const now = Date.now()
  setMessages((prev) => {
    const last = prev[prev.length - 1]
    if (last && last.role === 'assistant' && !last.done) {
      const blocks = last.blocks.map((b): MessageBlock => {
        if (b.type !== 'tool_call') return b
        const matches = callId ? b.id === callId : (b.name === name && b.output === undefined)
        return matches ? { ...b, output, error: isError, completedAt: now } : b
      })
      return [...prev.slice(0, -1), { ...last, blocks }]
    }
    return prev
  })
}

/**
 * Handle error message from server.
 */
export function handleError(
  data: Record<string, unknown>,
  authPending: React.MutableRefObject<boolean>,
  suppressReconnect: React.MutableRefObject<boolean>,
  setAuthFailed: (v: boolean) => void,
  setAuthValidating: (v: boolean) => void,
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>,
  setIsGenerating: (v: boolean) => void,
  currentAssistantId: React.MutableRefObject<string | null>,
  uid: () => string,
  authTokenKey: string,
) {
  const message = (data.message as string) || 'Unknown error'
  if (authPending.current) {
    authPending.current = false
    suppressReconnect.current = true
    setAuthFailed(true)
    setAuthValidating(false)
    try { localStorage.removeItem(authTokenKey) } catch { /* ignore */ }
    return
  }
  setMessages((prev) => [
    ...prev,
    { role: 'assistant', blocks: [{ type: 'content', text: `⚠️ Error: ${message}` }], id: uid(), done: true },
  ])
  setIsGenerating(false)
  currentAssistantId.current = null
}

/**
 * Handle message (non-streamed reply) from server.
 */
export function handleMessage(
  data: Record<string, unknown>,
  uid: () => string,
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>,
  setIsGenerating: (v: boolean) => void,
  currentAssistantId: React.MutableRefObject<string | null>,
) {
  const content = (data.content as string) || ''
  if (!content) return
  setMessages((prev) => {
    const last = prev[prev.length - 1]
    if (last && last.role === 'assistant' && !last.done) {
      const blocks = [...last.blocks]
      const lb = blocks[blocks.length - 1]
      if (lb?.type === 'content') {
        blocks[blocks.length - 1] = { type: 'content', text: lb.text ? `${lb.text}\n\n${content}` : content }
      } else {
        blocks.push({ type: 'content', text: content })
      }
      return [...prev.slice(0, -1), { ...last, blocks, done: true }]
    }
    return [...prev, { role: 'assistant', blocks: [{ type: 'content', text: content }], id: uid(), done: true }]
  })
  setIsGenerating(false)
  currentAssistantId.current = null
}

/**
 * Handle file message from server.
 */
export function handleFile(
  data: Record<string, unknown>,
  uid: () => string,
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>,
) {
  const fileName = (data.file_name as string) || 'file'
  const fileMime = (data.mime_type as string) || 'application/octet-stream'
  const fileData = (data.data as string) || ''
  const caption = (data.caption as string) || ''
  if (fileData) {
    const bin = atob(fileData)
    const bytes = new Uint8Array(bin.length)
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
    const blob = new Blob([bytes], { type: fileMime })
    const blobUrl = URL.createObjectURL(blob)
    const isImage = fileMime.startsWith('image/')
    const marker = isImage
      ? `[image: ${fileName}](${blobUrl})`
      : `[file: ${fileName}](${blobUrl})`
    const text = caption ? `${caption}\n${marker}` : marker
    setMessages((prev) => {
      const last = prev[prev.length - 1]
      if (last && last.role === 'assistant' && !last.done) {
        const blocks = [...last.blocks]
        const lb = blocks[blocks.length - 1]
        if (lb?.type === 'content') {
          blocks[blocks.length - 1] = { type: 'content', text: lb.text ? `${lb.text}\n\n${text}` : text }
        } else {
          blocks.push({ type: 'content', text })
        }
        return [...prev.slice(0, -1), { ...last, blocks }]
      }
      return [...prev, { role: 'assistant', blocks: [{ type: 'content', text }], id: uid(), done: false }]
    })
  }
}
