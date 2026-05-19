import { useEffect, useState, useCallback } from 'react'
import { Sparkles } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { Page, ErrorBanner, LoadingRow, EmptyState } from '../components/PageLayout'

interface Skill { name: string; description: string; keywords: string[] }

export default function Skills() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [skills, setSkills] = useState<Skill[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

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

  return (
    <Page
      icon={Sparkles}
      title="Skills"
      meta={skills.length ? ` · ${skills.length}` : undefined}
    >
      {error && <ErrorBanner message={error} />}
      {loading && <LoadingRow />}
      {!loading && status !== 'connected' && <EmptyState>Waiting for connection…</EmptyState>}
      {!loading && status === 'connected' && skills.length === 0 && (
        <EmptyState>No skills loaded. Add SKILL.md files under workspace/skills/.</EmptyState>
      )}
      {!loading && skills.length > 0 && (
        <div className="space-y-1.5">
          {skills.map((s) => (
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
    </Page>
  )
}
