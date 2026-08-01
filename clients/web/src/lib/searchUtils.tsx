import type { ChatMessage } from '../hooks/useWebSocket'
import type { ReactNode } from 'react'

export function searchableMessageText(msg: ChatMessage): string {
  if (msg.role === 'user') return msg.content
  return msg.blocks.map((b) => {
    if (b.type === 'content' || b.type === 'thinking') return b.text
    return [b.name, JSON.stringify(b.args), b.output || ''].join(' ')
  }).join(' ')
}

export function messageTimestamp(id: string): number | null {
  const m = id.match(/-(\d{13})$/)
  return m ? parseInt(m[1], 10) : null
}

export function timeDividerLabel(ts: number): string {
  const d = new Date(ts)
  const now = new Date()
  const sameDay = d.toDateString() === now.toDateString()
  const yesterday = new Date(now)
  yesterday.setDate(now.getDate() - 1)
  const prefix = sameDay ? 'Today' : d.toDateString() === yesterday.toDateString() ? 'Yesterday' : d.toLocaleDateString()
  return `${prefix} ${d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`
}

export function highlightText(text: string, query: string): ReactNode {
  if (!query) return text
  const regex = new RegExp(`(${query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')})`, 'gi')
  const parts = text.split(regex)
  if (parts.length === 1) return text
  return parts.map((part, i) =>
    regex.test(part) ? <mark key={i} className="bg-amber-500/30 text-amber-200 rounded px-0.5">{part}</mark> : part
  )
}

export function shouldShowTimeDivider(prev: ChatMessage | undefined, msg: ChatMessage): boolean {
  const ts = messageTimestamp(msg.id)
  if (!ts) return false
  const prevTs = prev ? messageTimestamp(prev.id) : null
  if (!prevTs) return true
  return ts - prevTs > 30 * 60 * 1000 || new Date(ts).toDateString() !== new Date(prevTs).toDateString()
}
