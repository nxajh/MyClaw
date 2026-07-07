import { useEffect, useMemo, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { Activity, AlertCircle, Clock, Cpu, Layers, MessageSquare, Wrench } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'

interface Session { id: string; name: string; created_at?: string; is_active?: boolean }
interface ModelInfo { id: string; active: boolean }
interface ToolInfo { name: string; description?: string }

function Card({ title, icon, children }: { title: string; icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="rounded-2xl border border-zinc-800 bg-zinc-900/50 p-4 shadow-sm">
      <div className="flex items-center gap-2 mb-3 text-sm font-semibold text-zinc-200">
        {icon}
        {title}
      </div>
      {children}
    </section>
  )
}

export default function Overview() {
  const { status, messages, request, activeSessionId } = useWebSocketContext()
  const navigate = useNavigate()
  const [sessions, setSessions] = useState<Session[]>([])
  const [models, setModels] = useState<ModelInfo[]>([])
  const [tools, setTools] = useState<ToolInfo[]>([])

  useEffect(() => {
    if (status !== 'connected') return
    Promise.allSettled([
      request('sessions.list'),
      request('models.list'),
      request('tools.list'),
    ]).then(([s, m, t]) => {
      if (s.status === 'fulfilled') setSessions((s.value as Session[]) || [])
      if (m.status === 'fulfilled') setModels(((m.value as any)?.models as ModelInfo[]) || [])
      if (t.status === 'fulfilled') setTools((t.value as ToolInfo[]) || [])
    })
  }, [status, request])

  const stats = useMemo(() => {
    let toolCalls = 0
    let failedTools = 0
    for (const msg of messages) {
      if (msg.role !== 'assistant') continue
      for (const block of msg.blocks) {
        if (block.type === 'tool_call') {
          toolCalls++
          if (block.error) failedTools++
        }
      }
    }
    return { toolCalls, failedTools, userMessages: messages.filter(m => m.role === 'user').length }
  }, [messages])

  const activeSession = sessions.find(s => s.is_active || s.id === activeSessionId)
  const activeModel = models.find(m => m.active)?.id ?? 'default'
  const recentSessions = sessions.slice(0, 5)

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="max-w-5xl mx-auto px-3 sm:px-6 py-5 sm:py-7 space-y-5">
        <div>
          <h1 className="text-xl font-semibold text-zinc-100">Overview</h1>
          <p className="text-sm text-zinc-500 mt-1">Current client status, recent sessions, and conversation activity.</p>
        </div>

        <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
          <div className={`rounded-2xl border p-4 bg-gradient-to-br transition-colors ${status === 'connected' ? 'border-emerald-800/40 from-emerald-950/20 to-zinc-900/40' : status === 'connecting' ? 'border-amber-800/40 from-amber-950/20 to-zinc-900/40' : 'border-red-800/40 from-red-950/20 to-zinc-900/40'}`}>
            <div className="flex items-center gap-1.5 text-xs text-zinc-500 mb-1.5">
              <Activity size={11} className={status === 'connected' ? 'text-emerald-400' : status === 'connecting' ? 'text-amber-400' : 'text-red-400'} /> Connection
            </div>
            <div className={`text-sm font-semibold ${status === 'connected' ? 'text-emerald-400' : status === 'connecting' ? 'text-amber-400' : 'text-red-400'}`}>{status}</div>
          </div>
          <div className="rounded-2xl border border-zinc-800 bg-gradient-to-br from-zinc-900/60 to-zinc-900/30 p-4">
            <div className="flex items-center gap-1.5 text-xs text-zinc-500 mb-1.5">
              <MessageSquare size={11} /> Messages
            </div>
            <div className="text-sm font-semibold text-zinc-200">{messages.length}</div>
          </div>
          <div className="rounded-2xl border border-zinc-800 bg-gradient-to-br from-zinc-900/60 to-zinc-900/30 p-4">
            <div className="flex items-center gap-1.5 text-xs text-zinc-500 mb-1.5">
              <Wrench size={11} /> Tool calls
            </div>
            <div className="text-sm font-semibold text-zinc-200">{stats.toolCalls}</div>
          </div>
          <div className={`rounded-2xl border p-4 bg-gradient-to-br transition-colors ${stats.failedTools > 0 ? 'border-red-800/40 from-red-950/20 to-zinc-900/40' : 'border-zinc-800 from-zinc-900/60 to-zinc-900/30'}`}>
            <div className="flex items-center gap-1.5 text-xs text-zinc-500 mb-1.5">
              <AlertCircle size={11} className={stats.failedTools > 0 ? 'text-red-400' : ''} /> Failures
            </div>
            <div className={`text-sm font-semibold ${stats.failedTools > 0 ? 'text-red-400' : 'text-zinc-200'}`}>{stats.failedTools}</div>
          </div>
        </div>

        <div className="grid lg:grid-cols-2 gap-4">
          <Card title="Current chat" icon={<MessageSquare size={15} className="text-zinc-500" />}>
            <div className="space-y-2 text-sm">
              <div className="flex justify-between gap-3"><span className="text-zinc-500">Session</span><span className="text-zinc-200 truncate">{activeSession?.name ?? 'No active session'}</span></div>
              <div className="flex justify-between gap-3"><span className="text-zinc-500">Model</span><span className="text-zinc-200 font-mono truncate">{activeModel}</span></div>
              <div className="flex justify-between gap-3"><span className="text-zinc-500">User turns</span><span className="text-zinc-200">{stats.userMessages}</span></div>
            </div>
          </Card>

          <Card title="Capabilities" icon={<Cpu size={15} className="text-zinc-500" />}>
            <div className="grid grid-cols-2 gap-2 text-sm">
              <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-3"><div className="text-xs text-zinc-500">Models</div><div className="text-zinc-200 font-semibold">{models.length}</div></div>
              <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-3"><div className="text-xs text-zinc-500">Tools</div><div className="text-zinc-200 font-semibold">{tools.length}</div></div>
            </div>
          </Card>

          <Card title="Recent sessions" icon={<Layers size={15} className="text-zinc-500" />}>
            <div className="space-y-1.5">
              {recentSessions.length === 0 && <p className="text-sm text-zinc-500">No sessions loaded.</p>}
              {recentSessions.map(s => (
                <button key={s.id} onClick={() => navigate('/sessions')} className="w-full flex items-center justify-between gap-2 rounded-xl px-3 py-2 text-left hover:bg-zinc-800/50 transition-colors">
                  <span className="text-sm text-zinc-300 truncate">{s.name}</span>
                  {s.created_at && <span className="text-[10px] text-zinc-600 shrink-0">{new Date(s.created_at).toLocaleDateString()}</span>}
                </button>
              ))}
            </div>
          </Card>

          <Card title="Recent tool health" icon={<Wrench size={15} className="text-zinc-500" />}>
            <div className="space-y-2 text-sm text-zinc-400">
              <div className="flex items-center gap-2"><Activity size={13} className="text-emerald-400" /> {stats.toolCalls} calls in current chat</div>
              <div className="flex items-center gap-2"><AlertCircle size={13} className={stats.failedTools ? 'text-red-400' : 'text-zinc-600'} /> {stats.failedTools} failed calls</div>
              <div className="flex items-center gap-2"><Clock size={13} className="text-zinc-600" /> Metrics are scoped to the loaded conversation</div>
            </div>
          </Card>
        </div>
      </div>
    </div>
  )
}
