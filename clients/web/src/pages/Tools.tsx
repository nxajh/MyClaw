import { useEffect, useState, useCallback, useMemo } from 'react'
import { Wrench, BookOpen, Terminal, Copy, Check, Search, Cpu } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from '../components/Toast'
import { ErrorBanner, EmptyState, searchInputCls } from '../components/PageLayout'

interface Tool {
  name: string
  description?: string
  parameters?: {
    type: string
    properties?: Record<string, {
      type: string
      description: string
      required?: boolean
      enum?: string[]
    }>
    required?: string[]
  }
}

function generateExampleJson(parameters: any): string {
  if (!parameters || !parameters.properties || Object.keys(parameters.properties).length === 0) {
    return '{}';
  }
  const obj: Record<string, any> = {};
  for (const [key, val] of Object.entries<any>(parameters.properties)) {
    if (val.type === 'string') {
      obj[key] = val.enum && val.enum.length > 0 ? val.enum[0] : 'string';
    } else if (val.type === 'integer' || val.type === 'number') {
      obj[key] = 0;
    } else if (val.type === 'boolean') {
      obj[key] = false;
    } else if (val.type === 'array') {
      obj[key] = [];
    } else if (val.type === 'object') {
      obj[key] = {};
    } else {
      obj[key] = null;
    }
  }
  return JSON.stringify(obj, null, 2);
}

interface ToolSpec {
  description: string
  parameters: {
    type: string
    properties: Record<string, {
      type: string
      description: string
      required?: boolean
      enum?: string[]
    }>
  }
  example: string
}

function generateToolSpec(tool: Tool): ToolSpec {
  const requiredSet = new Set(tool.parameters?.required || [])
  const properties: Record<string, { type: string; description: string; required?: boolean; enum?: string[] }> = {}
  if (tool.parameters?.properties) {
    for (const [key, val] of Object.entries(tool.parameters.properties)) {
      properties[key] = { ...val, required: val.required || requiredSet.has(key) }
    }
  }
  return {
    description: tool.description || 'No description available.',
    parameters: { type: tool.parameters?.type || 'object', properties },
    example: generateExampleJson(tool.parameters),
  }
}

export default function Tools() {
  const { status, request } = useWebSocketContext()
  const { toast } = useToast()
  const [tools, setTools] = useState<Tool[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const [searchQuery, setSearchQuery] = useState('')
  const [selectedToolName, setSelectedToolName] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)

  const fetchTools = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      const res = await request('tools.list')
      const fetched = (res as Tool[]) || []
      setTools(fetched)
      // Only auto-select first tool when nothing is selected yet
      setSelectedToolName((prev) => {
        if (prev && fetched.some((t) => t.name === prev)) return prev
        return fetched[0]?.name ?? null
      })
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  useEffect(() => {
    if (status === 'connected') fetchTools()
  }, [status, fetchTools])

  const filteredTools = useMemo(() => {
    const q = searchQuery.toLowerCase()
    return tools.filter(t =>
      t.name.toLowerCase().includes(q) ||
      (t.description || '').toLowerCase().includes(q)
    )
  }, [tools, searchQuery])

  useEffect(() => {
    if (filteredTools.length > 0) {
      const isStillVisible = filteredTools.some(t => t.name === selectedToolName)
      if (!isStillVisible) {
        setSelectedToolName(filteredTools[0].name)
      }
    }
  }, [filteredTools, selectedToolName])

  const activeToolSpec = useMemo<ToolSpec | null>(() => {
    if (!selectedToolName) return null
    const found = tools.find(t => t.name === selectedToolName)
    if (!found) return null
    return generateToolSpec(found)
  }, [tools, selectedToolName])

  const handleCopyExample = (exampleText: string) => {
    navigator.clipboard.writeText(exampleText)
    setCopied(true)
    toast('JSON copied to clipboard', 'success')
    setTimeout(() => setCopied(false), 2000)
  }

  const [mobileShowDetail, setMobileShowDetail] = useState(false)

  return (
    <div className="flex flex-col h-full bg-zinc-950 page-enter">
      <div className="flex-1 flex flex-col md:flex-row overflow-hidden">
        {/* Left Side: Tool List */}
        <div className={`${mobileShowDetail ? 'hidden md:flex' : 'flex'} w-full md:w-80 border-b md:border-b-0 md:border-r border-zinc-800 flex-col shrink-0`}>
          <div className="sticky top-0 z-10 p-3 sm:p-4 border-b border-zinc-800 space-y-2.5 bg-zinc-950/90 backdrop-blur-md">
            <div>
              <h1 className="text-lg font-semibold tracking-tight text-zinc-100 flex items-center gap-2">
                <Cpu size={16} className="text-zinc-500" />
                Active MCP Tools
              </h1>
              <p className="text-sm text-zinc-500 mt-0.5">{tools.length} registered · Inspect schemas</p>
            </div>

            <div className="relative">
              <span className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                <Search size={13} className="text-zinc-500" />
              </span>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search tools..."
                className={searchInputCls + ' !rounded-xl !pl-9 !py-2'}
              />
            </div>
          </div>

          <div className="flex-1 overflow-y-auto p-2 space-y-0.5">
            {error && <div className="px-2"><ErrorBanner message={error} /></div>}
            {loading && tools.length === 0 && (
              <div className="px-1 space-y-1">
                {Array.from({ length: 8 }).map((_, i) => (
                  <div key={i} className="rounded-xl border border-zinc-800 bg-zinc-900/40 px-3 py-2.5 space-y-1.5">
                    <div className="skeleton-line w-2/3 h-2.5" />
                    <div className="skeleton-line w-full h-2" />
                  </div>
                ))}
              </div>
            )}
            {!loading && status !== 'connected' && <EmptyState>Waiting for sync…</EmptyState>}
            {!loading && status === 'connected' && filteredTools.length === 0 && (
              <div className="px-2 py-4">
                <EmptyState
                  icon={<Wrench size={24} />}
                  action={searchQuery ? (
                    <button onClick={() => setSearchQuery('')} className="text-sm text-blue-400 hover:text-blue-300">Clear filter</button>
                  ) : undefined}
                >
                  {searchQuery ? `No tools match “${searchQuery}”` : 'No tools registered'}
                </EmptyState>
              </div>
            )}

            {filteredTools.length > 0 && (
              filteredTools.map((tool) => {
                const isSelected = tool.name === selectedToolName
                return (
                  <button
                    key={tool.name}
                    onClick={() => { setSelectedToolName(tool.name); setMobileShowDetail(true) }}
                    className={`w-full flex flex-col gap-0.5 px-3 py-2.5 text-left rounded-xl transition-all relative ${
                      isSelected ? 'bg-zinc-900 text-zinc-100 shadow-sm' : 'text-zinc-400 hover:text-zinc-200 hover:bg-zinc-900/40'
                    }`}
                  >
                    {isSelected && (
                      <div className="absolute left-0 top-1.5 bottom-1.5 w-0.5 rounded-r bg-zinc-400" />
                    )}
                    <div className="flex items-center gap-2 min-w-0">
                      <Wrench size={13} className={`shrink-0 ${isSelected ? 'text-zinc-300' : 'text-zinc-600'}`} />
                      <span className="font-mono text-sm truncate font-medium">{tool.name}</span>
                    </div>
                    {tool.description && (
                      <p className={`pl-5 text-xs leading-snug line-clamp-2 ${isSelected ? 'text-zinc-400' : 'text-zinc-600'}`}>
                        {tool.description}
                      </p>
                    )}
                  </button>
                )
              })
            )}
          </div>
        </div>

        {/* Right Side: detail panel */}
        <div className={`${mobileShowDetail ? 'flex' : 'hidden md:flex'} flex-1 overflow-y-auto bg-zinc-950 flex-col`}>
          {mobileShowDetail && (
            <button
              onClick={() => setMobileShowDetail(false)}
              className="md:hidden mx-3 mt-3 flex items-center gap-1.5 text-sm text-zinc-400 hover:text-zinc-200 transition-colors"
            >
              ← Back to tools
            </button>
          )}
          {selectedToolName ? (
            <div className="flex flex-col min-h-full">
              {/* Sticky tool header */}
              <div className="sticky top-0 z-10 border-b border-zinc-800 bg-zinc-950/90 backdrop-blur-md px-3 sm:px-8 py-4 space-y-1.5">
                <div className="flex items-center gap-1.5 text-xs text-zinc-400 font-semibold uppercase tracking-wider bg-zinc-900/60 border border-zinc-800 px-2 py-0.5 rounded-md max-w-fit">
                  <Wrench size={11} /> Active Tool Schema
                </div>
                <h1 className="text-lg font-semibold tracking-tight text-zinc-100 font-mono">
                  {selectedToolName}
                </h1>
                <p className="text-sm text-zinc-400 leading-relaxed">
                  {activeToolSpec ? activeToolSpec.description : 'System-registered runtime dynamic extension tool.'}
                </p>
              </div>

              <div className="px-3 sm:px-8 py-5 space-y-6">
                {/* Parameters Table */}
                <div className="space-y-3">
                  <h2 className="text-sm font-medium text-zinc-300 flex items-center gap-1.5">
                    <BookOpen size={14} className="text-zinc-500" />
                    Parameters Definition
                  </h2>

                  {activeToolSpec && Object.keys(activeToolSpec.parameters.properties).length > 0 ? (
                    <div className="rounded-2xl border border-zinc-800 bg-zinc-900/50 overflow-hidden text-sm shadow-sm">
                      <div className="hidden sm:grid grid-cols-4 bg-zinc-900/60 border-b border-zinc-800 px-4 py-2.5 text-zinc-500 font-medium text-xs">
                        <div className="col-span-1">Field</div>
                        <div className="col-span-1 text-center">Type</div>
                        <div className="col-span-2">Description</div>
                      </div>
                      <div className="divide-y divide-zinc-800">
                        {Object.entries(activeToolSpec.parameters.properties).map(([name, val]) => (
                          <div key={name} className="grid grid-cols-2 sm:grid-cols-4 px-3 sm:px-4 py-3 items-baseline gap-1 sm:gap-0">
                            <div className="col-span-2 sm:col-span-1 font-mono text-sm text-zinc-200 font-medium flex items-center gap-1.5">
                              {val.required && (
                                <span className="h-1.5 w-1.5 rounded-full bg-red-500 shrink-0" title="Required" />
                              )}
                              {name}
                              {val.required && <span className="text-red-400 text-xs font-sans font-semibold">req</span>}
                            </div>
                            <div className="col-span-2 sm:col-span-1 text-left sm:text-center">
                              <span className="text-xs bg-zinc-900 border border-zinc-800 text-zinc-400 font-semibold px-2 py-0.5 rounded-md font-mono">
                                {val.type}
                              </span>
                            </div>
                            <div className="col-span-2 sm:col-span-2 space-y-1.5 mt-1 sm:mt-0">
                              <p className="text-zinc-400 text-sm leading-relaxed">{val.description}</p>
                              {val.enum && (
                                <div className="flex flex-wrap items-center gap-1">
                                  <span className="text-xs text-zinc-500 font-medium">Enum:</span>
                                  {val.enum.map(e => (
                                    <span key={e} className="text-xs font-mono text-zinc-400 bg-zinc-900 border border-zinc-800 px-1.5 py-0.5 rounded-sm">
                                      "{e}"
                                    </span>
                                  ))}
                                </div>
                              )}
                            </div>
                          </div>
                        ))}
                      </div>
                    </div>
                  ) : (
                    <div className="rounded-2xl border border-zinc-800 bg-zinc-900/50 p-5 text-center text-sm text-zinc-500 leading-normal">
                      This tool takes no parameters. Input args are passed as an empty JSON object `{}`.
                    </div>
                  )}
                </div>

                {/* Execution Sample */}
                {activeToolSpec && (
                  <div className="space-y-2.5">
                    <div className="flex items-center justify-between">
                      <h2 className="text-sm font-medium text-zinc-300 flex items-center gap-1.5">
                        <Terminal size={14} className="text-emerald-400" />
                        Argument JSON Example
                      </h2>
                      <button
                        onClick={() => handleCopyExample(activeToolSpec.example)}
                        className="flex items-center gap-1 px-2.5 py-1.5 rounded-lg border border-zinc-800 hover:bg-zinc-900/60 text-xs text-zinc-500 hover:text-zinc-300 transition-colors shrink-0"
                      >
                        {copied ? (
                          <>
                            <Check size={12} className="text-emerald-400" /> Copied
                          </>
                        ) : (
                          <>
                            <Copy size={12} /> Copy JSON
                          </>
                        )}
                      </button>
                    </div>
                    <pre className="bg-zinc-950 border border-zinc-800 p-4 rounded-2xl text-xs text-zinc-300 font-mono overflow-x-auto leading-relaxed shadow-sm">
                      {activeToolSpec.example}
                    </pre>
                  </div>
                )}
              </div>
            </div>
          ) : (
            <div className="h-full flex items-center justify-center text-sm text-zinc-600">
              Select a tool on the left to inspect its parameters and JSON definitions.
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
