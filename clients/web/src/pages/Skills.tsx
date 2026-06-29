import { useEffect, useState, useCallback, useMemo } from 'react'
import { Sparkles, Search } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { ErrorBanner, LoadingRow, EmptyState } from '../components/PageLayout'

interface Skill { name: string; description: string; keywords: string[] }

export default function Skills() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [skills, setSkills] = useState<Skill[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [searchQuery, setSearchQuery] = useState('')

  const fetchSkills = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      setSkills((await request('skills.list') as Skill[]) || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  useEffect(() => { if (status === 'connected') fetchSkills() }, [status, fetchSkills])

  const filteredSkills = useMemo(() => {
    if (!searchQuery.trim()) return skills
    const q = searchQuery.toLowerCase()
    return skills.filter(s =>
      s.name.toLowerCase().includes(q) ||
      s.description?.toLowerCase().includes(q) ||
      s.keywords?.some(k => k.toLowerCase().includes(q))
    )
  }, [skills, searchQuery])

  return (
    <div className="flex flex-col h-full">
      <div className="flex-1 overflow-y-auto">
        <div className="max-w-2xl mx-auto px-3 sm:px-6 py-4 sm:py-6 space-y-4">
          {/* Header */}
          <div className="pb-2 border-b border-zinc-900">
            <h1 className="text-base font-bold text-zinc-100 flex items-center gap-1.5">
              <Sparkles size={14} className="text-amber-400" />
              Skills Library
            </h1>
            <p className="text-xs text-zinc-500 mt-0.5">Loaded behavior templates guiding MyClaw's capabilities</p>
          </div>

          {/* Search */}
          {skills.length > 0 && (
            <div className="relative">
              <span className="absolute inset-y-0 left-0 pl-3.5 flex items-center pointer-events-none">
                <Search size={14} className="text-zinc-500" />
              </span>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search skills by name, description, or keyword…"
                className="w-full rounded-2xl border border-zinc-900 bg-zinc-900/30 pl-10 pr-4 py-2.5 text-xs text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-800 transition-colors"
              />
            </div>
          )}

          {error && <ErrorBanner message={error} />}
          {loading && <LoadingRow />}
          {!loading && status !== 'connected' && <EmptyState>Waiting for connection…</EmptyState>}
          {!loading && status === 'connected' && skills.length === 0 && (
            <EmptyState>No skills loaded. Add SKILL.md files under workspace/skills/.</EmptyState>
          )}
          {!loading && status === 'connected' && skills.length > 0 && filteredSkills.length === 0 && (
            <div className="text-center py-8">
              <p className="text-xs text-zinc-600">No skills match "{searchQuery}"</p>
              <button onClick={() => setSearchQuery('')} className="text-xs text-blue-400 hover:text-blue-300 mt-1">Clear filter</button>
            </div>
          )}
          {!loading && filteredSkills.length > 0 && (
            <div className="space-y-1.5">
              {filteredSkills.map((s) => (
                <div
                  key={s.name}
                  className="rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3"
                >
                  <div className="flex items-center gap-2">
                    <Sparkles size={13} className="text-amber-400 shrink-0" />
                    <span className="font-mono text-sm text-zinc-200">{s.name}</span>
                  </div>
                  {s.description && (
                    <p className="mt-1.5 text-sm text-zinc-400 leading-relaxed">{s.description}</p>
                  )}
                  {s.keywords?.length > 0 && (
                    <div className="mt-2 flex flex-wrap gap-1.5">
                      {s.keywords.map((k) => (
                        <span key={k} className="px-2 py-0.5 rounded-md bg-zinc-800 text-[11px] text-zinc-500">
                          {k}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
