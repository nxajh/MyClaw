import { useEffect, useState, useCallback } from 'react'
import { Settings, Save, RotateCcw, Loader2, AlertTriangle } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { Page, ErrorBanner, LoadingRow, btnPrimary, btnGhost } from '../components/PageLayout'

interface ConfigMeta {
  tool_count: number
  workspace_dir: string | null
  config_path: string | null
}

interface RawConfig {
  content: string
  path: string
}

export default function Config() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [meta, setMeta] = useState<ConfigMeta | null>(null)
  const [raw, setRaw] = useState<RawConfig | null>(null)
  const [draft, setDraft] = useState('')
  const [loading, setLoading] = useState(false)
  const [saving, setSaving] = useState(false)
  const [restarting, setRestarting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

  const fetchAll = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      const [metaRes, rawRes] = await Promise.all([
        request('config.get') as Promise<ConfigMeta>,
        request('config.get_raw') as Promise<RawConfig>,
      ])
      setMeta(metaRes)
      setRaw(rawRes)
      setDraft(rawRes.content)
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  const handleSave = useCallback(async () => {
    setSaving(true)
    setError(null)
    setSaved(false)
    try {
      await request('config.save', { content: draft })
      setSaved(true)
      setTimeout(() => setSaved(false), 3000)
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSaving(false)
    }
  }, [request, draft])

  const handleRestart = useCallback(async () => {
    setRestarting(true)
    setError(null)
    try {
      await request('daemon.restart')
      // Connection will drop; reconnect logic handles it automatically.
    } catch {
      // Expected — connection may close before we get a response.
    } finally {
      setTimeout(() => setRestarting(false), 5000)
    }
  }, [request])

  useEffect(() => { if (status === 'connected') fetchAll() }, [status, fetchAll])

  const isDirty = raw ? draft !== raw.content : false

  return (
    <Page
      icon={Settings}
      title="Config"
      actions={
        <div className="flex items-center gap-2">
          {saved && <span className="text-xs text-emerald-400">Saved</span>}
          <button
            onClick={handleSave}
            disabled={saving || !isDirty || status !== 'connected'}
            className={btnPrimary}
          >
            {saving ? <Loader2 size={13} className="animate-spin" /> : <Save size={13} />}
            Save
          </button>
          <button
            onClick={handleRestart}
            disabled={restarting || status !== 'connected'}
            className={btnGhost}
            title="Save first, then restart to apply changes"
          >
            {restarting ? <Loader2 size={13} className="animate-spin" /> : <RotateCcw size={13} />}
            Restart
          </button>
        </div>
      }
    >
      {error && <ErrorBanner message={error} />}
      {loading && <LoadingRow label="Loading config…" />}

      {!loading && meta && (
        <div className="grid grid-cols-2 gap-2 text-xs">
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3">
            <div className="text-zinc-600 mb-0.5">Tools</div>
            <div className="text-zinc-300 font-mono">{meta.tool_count}</div>
          </div>
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3 min-w-0">
            <div className="text-zinc-600 mb-0.5">Config file</div>
            <div className="text-zinc-300 font-mono truncate" title={meta.config_path ?? ''}>
              {meta.config_path ?? '—'}
            </div>
          </div>
        </div>
      )}

      {!loading && raw && (
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <span className="text-xs text-zinc-500">myclaw.toml</span>
            {isDirty && (
              <span className="flex items-center gap-1 text-xs text-amber-400">
                <AlertTriangle size={11} /> Unsaved changes · Save then Restart to apply
              </span>
            )}
          </div>
          <textarea
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            rows={28}
            spellCheck={false}
            className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3.5 text-xs font-mono text-zinc-300 outline-none focus:border-zinc-700 resize-y transition-colors leading-5"
          />
        </div>
      )}

      {!loading && status === 'connected' && !raw && !error && (
        <p className="text-sm text-zinc-500">Config file not accessible.</p>
      )}
      {!loading && status !== 'connected' && (
        <p className="text-sm text-zinc-500">Waiting for connection…</p>
      )}
    </Page>
  )
}
