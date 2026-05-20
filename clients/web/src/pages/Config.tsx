import { useEffect, useState, useCallback, useMemo } from 'react'
import { Save, RotateCcw, Loader2, AlertTriangle, Settings, Code, FileText, Globe } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { ErrorBanner, LoadingRow, btnPrimary, btnGhost, inputCls } from '../components/PageLayout'

interface ConfigMeta {
  tool_count: number
  workspace_dir: string | null
  config_path: string | null
}

interface RawConfig {
  content: string
  path: string
}

// ── TOML Simple Regex Parser & Injector ──────────────────────────────────────

const KNOWLEDGE_DIR_REG = /^knowledge_dir\s*=\s*"([^"]*)"/m
const PORT_REG = /^port\s*=\s*(\d+)/m

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

  // Editor View Mode
  const [viewMode, setViewMode] = useState<'form' | 'code'>('form')

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
      // Keep dirty state clean on successful save
      setRaw(prev => prev ? { ...prev, content: draft } : prev)
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
    } catch {
      // Expected — connection may close before response.
    } finally {
      setTimeout(() => setRestarting(false), 5000)
    }
  }, [request])

  useEffect(() => { if (status === 'connected') fetchAll() }, [status, fetchAll])

  const isDirty = raw ? draft !== raw.content : false

  useEffect(() => {
    (window as any).myclawUnsaved = isDirty
    return () => {
      (window as any).myclawUnsaved = false
    }
  }, [isDirty])

  useEffect(() => {
    if (!isDirty) return
    const handleBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      e.returnValue = 'You have unsaved changes. Are you sure you want to leave?'
      return e.returnValue
    }
    window.addEventListener('beforeunload', handleBeforeUnload)
    return () => window.removeEventListener('beforeunload', handleBeforeUnload)
  }, [isDirty])

  // Extract Form parameters from the raw draft content
  const formParams = useMemo(() => {
    const knowledgeDirMatch = draft.match(KNOWLEDGE_DIR_REG)
    const portMatch = draft.match(PORT_REG)
    return {
      knowledgeDir: knowledgeDirMatch ? knowledgeDirMatch[1] : '',
      port: portMatch ? portMatch[1] : ''
    }
  }, [draft])

  // Mutate TOML draft content from Form inputs
  const handleUpdateFormParam = (key: 'knowledge_dir' | 'port', value: string) => {
    let newDraft = draft
    if (key === 'knowledge_dir') {
      if (KNOWLEDGE_DIR_REG.test(draft)) {
        newDraft = draft.replace(KNOWLEDGE_DIR_REG, `knowledge_dir = "${value}"`)
      } else {
        // Find insert place or append
        newDraft = `knowledge_dir = "${value}"\n` + draft
      }
    } else if (key === 'port') {
      const portVal = parseInt(value, 10) || 0
      if (PORT_REG.test(draft)) {
        newDraft = draft.replace(PORT_REG, `port = ${portVal}`)
      } else {
        newDraft = `port = ${portVal}\n` + draft
      }
    }
    setDraft(newDraft)
  }

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Tab') {
      e.preventDefault()
      const target = e.currentTarget
      const start = target.selectionStart
      const end = target.selectionEnd
      const val = target.value
      const newVal = val.substring(0, start) + "  " + val.substring(end)
      setDraft(newVal)
      setTimeout(() => {
        target.selectionStart = target.selectionEnd = start + 2
      }, 0)
    }
  }

  return (
    <div className="flex flex-col h-full bg-zinc-950">
      <div className="flex-1 overflow-y-auto">
        <div className="max-w-2xl mx-auto px-6 py-6 space-y-5 animate-fadeIn">

          {/* Action Row & Diagnostics */}
          <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3 pb-3 border-b border-zinc-900">
            <div>
              <h1 className="text-base font-bold text-zinc-100 flex items-center gap-1.5">
                <Settings size={15} className="text-blue-400" />
                Configuration Center
              </h1>
              <p className="text-xs text-zinc-500">Fine-tune MyClaw system parameters and runtime daemons</p>
            </div>
            
            <div className="flex items-center gap-2">
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
          </div>

          {/* Diagnostics banner */}
          <div className="flex items-center justify-between bg-zinc-900/10 border border-zinc-900 px-4 py-3 rounded-2xl">
            <div className="flex items-center gap-2">
              {isDirty ? (
                <span className="flex items-center gap-1.5 text-xs text-amber-400 font-medium">
                  <AlertTriangle size={12} className="animate-pulse" />
                  Unsaved Changes · Click "Save" and "Restart" to apply
                </span>
              ) : (
                <span className="text-xs text-zinc-500 font-medium">
                  All changes applied
                </span>
              )}
            </div>
            {saved && <span className="text-xs text-emerald-400 font-medium animate-pulse">✓ Saved Successfully</span>}
          </div>

          {error && <ErrorBanner message={error} />}
          {loading && <LoadingRow label="Loading configurations…" />}

          {/* Configuration Metadata Blocks */}
          {!loading && meta && (
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3 text-xs">
              <div className="rounded-2xl border border-zinc-900 bg-zinc-900/10 px-4 py-3.5 space-y-1">
                <div className="text-zinc-600 font-medium flex items-center gap-1">
                  <Globe size={11} /> Loaded Tools
                </div>
                <div className="text-zinc-300 font-semibold font-mono text-sm">{meta.tool_count} builtins</div>
              </div>
              <div className="rounded-2xl border border-zinc-900 bg-zinc-900/10 px-4 py-3.5 space-y-1 min-w-0">
                <div className="text-zinc-600 font-medium flex items-center gap-1">
                  <FileText size={11} /> Config File Path
                </div>
                <div className="text-zinc-300 font-mono text-[11px] truncate" title={meta.config_path ?? ''}>
                  {meta.config_path ?? '—'}
                </div>
              </div>
            </div>
          )}

          {/* View Switcher Tabs */}
          {!loading && raw && (
            <div className="flex bg-zinc-950 border border-zinc-900 rounded-xl p-0.5 self-start text-xs max-w-fit">
              <button
                onClick={() => setViewMode('form')}
                className={`flex items-center gap-1.5 px-3 py-1.5 rounded-lg transition-colors ${viewMode === 'form' ? 'bg-zinc-900 text-zinc-100 font-semibold' : 'text-zinc-500 hover:text-zinc-300'}`}
              >
                <Settings size={12} />
                Parameters UI
              </button>
              <button
                onClick={() => setViewMode('code')}
                className={`flex items-center gap-1.5 px-3 py-1.5 rounded-lg transition-colors ${viewMode === 'code' ? 'bg-zinc-900 text-zinc-100 font-semibold' : 'text-zinc-500 hover:text-zinc-300'}`}
              >
                <Code size={12} />
                TOML Source Code
              </button>
            </div>
          )}

          {/* Editor Body */}
          {!loading && raw && (
            <div className="space-y-4">
              {viewMode === 'form' ? (
                <div className="space-y-4 bg-zinc-900/20 border border-zinc-900 p-5 rounded-2xl">
                  <div className="space-y-1.5">
                    <label className="text-xs font-semibold text-zinc-400">Knowledge Workspace Directory</label>
                    <input
                      type="text"
                      value={formParams.knowledgeDir}
                      onChange={(e) => handleUpdateFormParam('knowledge_dir', e.target.value)}
                      placeholder="e.g. /home/ubuntu/.myclaw/workspace"
                      className={inputCls}
                    />
                    <p className="text-[10px] text-zinc-500">The absolute path where memory and logs are located</p>
                  </div>

                  <div className="space-y-1.5">
                    <label className="text-xs font-semibold text-zinc-400">Daemon Listen Port</label>
                    <input
                      type="number"
                      value={formParams.port}
                      onChange={(e) => handleUpdateFormParam('port', e.target.value)}
                      placeholder="e.g. 8080"
                      className={inputCls}
                    />
                    <p className="text-[10px] text-zinc-500">Port number the backend daemon service runs on</p>
                  </div>

                  <div className="pt-2 border-t border-zinc-900 text-[10px] text-zinc-500 leading-normal flex items-center gap-1.5">
                    <AlertTriangle size={11} className="text-amber-500 shrink-0" />
                    Additional advanced parameters can be adjusted via the "TOML Source Code" mode.
                  </div>
                </div>
              ) : (
                <div className="space-y-1.5">
                  <div className="flex justify-between items-center">
                    <span className="text-xs text-zinc-500 font-mono">myclaw.toml</span>
                  </div>
                  <textarea
                    value={draft}
                    onChange={(e) => setDraft(e.target.value)}
                    onKeyDown={handleKeyDown}
                    rows={22}
                    spellCheck={false}
                    className="w-full rounded-2xl border border-zinc-900 bg-zinc-950 px-4 py-3.5 text-xs font-mono text-zinc-300 outline-none focus:border-zinc-800 resize-y transition-colors leading-relaxed"
                  />
                </div>
              )}
            </div>
          )}

          {!loading && status === 'connected' && !raw && !error && (
            <p className="text-xs text-zinc-500">Config file not accessible.</p>
          )}
          {!loading && status !== 'connected' && (
            <p className="text-xs text-zinc-500">Waiting for connection…</p>
          )}

        </div>
      </div>
    </div>
  )
}