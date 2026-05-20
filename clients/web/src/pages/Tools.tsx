import { useEffect, useState, useCallback, useMemo } from 'react'
import { Wrench, BookOpen, Terminal, Copy, Check, Search, Cpu } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { ErrorBanner, LoadingRow, EmptyState } from '../components/PageLayout'

interface Tool {
  name: string
}

// ── Builtin Tool Registry Database ───────────────────────────────────────────

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

const toolRegistry: Record<string, ToolSpec> = {
  memory_list: {
    description: 'List all persistent memory entries and core facts with comprehensive metadata.',
    parameters: {
      type: 'object',
      properties: {}
    },
    example: '{}'
  },
  memory_view: {
    description: 'Read the full contents of a specific persistent memory file by its unique name.',
    parameters: {
      type: 'object',
      properties: {
        name: {
          type: 'string',
          description: 'Unique file-based identifier of the memory entry (without .md extension).',
          required: true
        }
      }
    },
    example: '{\n  "name": "myclaw_development_preferences"\n}'
  },
  memory_search: {
    description: 'Perform keyword-based semantic search across name, summary, and contents of all memory entries.',
    parameters: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Keyword search query matching abstract headers or bodies.',
          required: true
        }
      }
    },
    example: '{\n  "query": "qqbot token"\n}'
  },
  memory_manage: {
    description: 'Directly add, replace, or remove persistent memory records with frontmatter validation.',
    parameters: {
      type: 'object',
      properties: {
        action: {
          type: 'string',
          description: 'Action execution type.',
          required: true,
          enum: ['add', 'replace', 'remove']
        },
        name: {
          type: 'string',
          description: 'Unique memory identifier key.',
          required: true
        },
        content: {
          type: 'string',
          description: 'Memory markdown body (required for add / replace).'
        },
        memory_type: {
          type: 'string',
          description: 'Scope classification.',
          enum: ['user', 'feedback', 'project', 'reference']
        },
        abstract: {
          type: 'string',
          description: 'Brief 1-2 sentence header summary.'
        },
        tags: {
          type: 'array',
          description: 'Category keyword tag tags list, e.g. ["rust", "qqbot"].'
        }
      }
    },
    example: '{\n  "action": "add",\n  "name": "user_preferences",\n  "memory_type": "user",\n  "content": "User prefers concise summaries in Chinese.",\n  "abstract": "User localization preferences."\n}'
  },
  web_search: {
    description: 'Search the web for real-time information, news, or technical documentation using external APIs.',
    parameters: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Search phrase or keywords.',
          required: true
        },
        limit: {
          type: 'integer',
          description: 'Maximum search snippets to return (defaults to 5).'
        }
      }
    },
    example: '{\n  "query": "Rust actix-web websocket configuration 2026"\n}'
  },
  file_read: {
    description: 'Read the contents of a local workspace file with optional offset limit pagination.',
    parameters: {
      type: 'object',
      properties: {
        path: {
          type: 'string',
          description: 'Absolute or relative workspace path of target file.',
          required: true
        },
        limit: {
          type: 'integer',
          description: 'Maximum text lines to return (defaults to read all).'
        },
        offset: {
          type: 'integer',
          description: '1-based starting line number offset.'
        }
      }
    },
    example: '{\n  "path": "MyClaw/src/channels/client.rs",\n  "limit": 100\n}'
  },
  file_edit: {
    description: 'Apply high-precision atomic modification by replacing an exact string pattern in a target workspace file.',
    parameters: {
      type: 'object',
      properties: {
        path: {
          type: 'string',
          description: 'Path to target source file.',
          required: true
        },
        old_string: {
          type: 'string',
          description: 'Exact text segment to search for (must exist exactly once).',
          required: true
        },
        new_string: {
          type: 'string',
          description: 'Replacement string value (pass empty string to delete old segment).',
          required: true
        }
      }
    },
    example: '{\n  "path": "clients/web/src/App.tsx",\n  "old_string": "const hasToken = false",\n  "new_string": "const hasToken = true"\n}'
  },
  cronjob: {
    description: 'Schedule, update, pause, or run background automation tasks with retry policies.',
    parameters: {
      type: 'object',
      properties: {
        action: {
          type: 'string',
          description: 'Scheduler operation action type.',
          required: true,
          enum: ['create', 'update', 'list', 'pause', 'resume', 'run', 'remove', 'log']
        },
        id: {
          type: 'string',
          description: 'Unique task scheduler registration identifier.'
        },
        schedule: {
          type: 'string',
          description: 'Cron format string (sec min hour day month weekday) or interval specifier e.g. "every 30m".'
        },
        prompt: {
          type: 'string',
          description: 'Instruction prompt delivered to the agent model on trigger.'
        }
      }
    },
    example: '{\n  "action": "create",\n  "name": "system_heartbeat",\n  "schedule": "every 5m",\n  "prompt": "Check all system metrics and print a diagnostics log."\n}'
  },
  skill_view: {
    description: 'Inspect full instruction files or supporting asset files inside the skill repository.',
    parameters: {
      type: 'object',
      properties: {
        name: {
          type: 'string',
          description: 'Skill folder name identifier.',
          required: true
        },
        file_path: {
          type: 'string',
          description: 'Supporting file path under references/ scripts/ templates/.'
        }
      }
    },
    example: '{\n  "name": "github",\n  "file_path": "scripts/run.py"\n}'
  }
}

export default function Tools() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [tools, setTools] = useState<Tool[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  
  // Search & Navigation
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
      if (fetched.length > 0 && !selectedToolName) {
        setSelectedToolName(fetched[0].name)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request, selectedToolName])

  useEffect(() => {
    if (status === 'connected') fetchTools()
  }, [status, fetchTools])

  // Filter tools based on search query
  const filteredTools = useMemo(() => {
    return tools.filter(t => 
      t.name.toLowerCase().includes(searchQuery.toLowerCase())
    )
  }, [tools, searchQuery])

  // Select first match when filtering tools
  useEffect(() => {
    if (filteredTools.length > 0) {
      const isStillVisible = filteredTools.some(t => t.name === selectedToolName)
      if (!isStillVisible) {
        setSelectedToolName(filteredTools[0].name)
      }
    }
  }, [filteredTools, selectedToolName])

  // Retrieve current active tool spec
  const activeToolSpec = useMemo<ToolSpec | null>(() => {
    if (!selectedToolName) return null
    
    // Check local registry (handles both raw names and minimax__ prefixed ones)
    const cleanName = selectedToolName.includes('__') 
      ? selectedToolName.split('__')[1] 
      : selectedToolName
      
    return toolRegistry[cleanName] || null
  }, [selectedToolName])

  const handleCopyExample = (exampleText: string) => {
    navigator.clipboard.writeText(exampleText)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  return (
    <div className="flex flex-col h-full bg-zinc-950">
      <div className="flex-1 flex overflow-hidden">
        {/* Left Side: Tool List */}
        <div className="w-80 border-r border-zinc-900 flex flex-col shrink-0">
          <div className="p-4 border-b border-zinc-900 space-y-2.5">
            <div>
              <h1 className="text-sm font-bold text-zinc-100 flex items-center gap-1.5">
                <Cpu size={14} className="text-amber-400" />
                Active MCP Tools
              </h1>
              <p className="text-[10px] text-zinc-500">Inspect registered schemas and Swagger endpoints</p>
            </div>
            
            {/* Search filter input */}
            <div className="relative">
              <span className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                <Search size={12} className="text-zinc-600" />
              </span>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search tools..."
                className="w-full rounded-xl border border-zinc-900 bg-zinc-900/30 pl-8 pr-3 py-1.5 text-xs text-zinc-300 placeholder-zinc-650 outline-none focus:border-zinc-800 transition-colors"
              />
            </div>
          </div>

          <div className="flex-1 overflow-y-auto p-2 space-y-0.5">
            {error && <div className="px-2"><ErrorBanner message={error} /></div>}
            {loading && <div className="px-2"><LoadingRow /></div>}
            {!loading && status !== 'connected' && <EmptyState>Waiting for sync…</EmptyState>}
            {!loading && status === 'connected' && filteredTools.length === 0 && (
              <div className="text-center py-8 text-xs text-zinc-600">No tools registered</div>
            )}
            
            {!loading && filteredTools.length > 0 && (
              filteredTools.map((tool) => {
                const isSelected = tool.name === selectedToolName
                return (
                  <button
                    key={tool.name}
                    onClick={() => setSelectedToolName(tool.name)}
                    className={`w-full flex items-center gap-2.5 px-3 py-2 text-left rounded-xl transition-all relative ${
                      isSelected ? 'bg-zinc-900 text-zinc-100' : 'text-zinc-400 hover:text-zinc-200 hover:bg-zinc-900/30'
                    }`}
                  >
                    {isSelected && (
                      <div className="absolute left-0 top-1.5 bottom-1.5 w-0.5 rounded-r bg-amber-400" />
                    )}
                    <Wrench size={12} className={`shrink-0 ${isSelected ? 'text-amber-400' : 'text-zinc-600'}`} />
                    <span className="font-mono text-[11px] truncate">{tool.name}</span>
                  </button>
                )
              })
            )}
          </div>
        </div>

        {/* Right Side: Swagger Spec Showcase Panel */}
        <div className="flex-1 overflow-y-auto bg-zinc-950 p-6">
          {selectedToolName ? (
            <div className="max-w-2xl mx-auto space-y-6 animate-fadeIn">
              
              {/* Header Showcase */}
              <div className="border-b border-zinc-900 pb-4 space-y-1">
                <div className="flex items-center gap-1.5 text-[10px] text-amber-400 font-bold uppercase tracking-wider bg-amber-500/10 border border-amber-500/20 px-2 py-0.5 rounded-md max-w-fit">
                  <Wrench size={10} /> Active Tool Schema
                </div>
                <h1 className="text-base font-bold text-zinc-100 font-mono tracking-tight pt-1">
                  {selectedToolName}
                </h1>
                <p className="text-xs text-zinc-400 pt-1 leading-relaxed">
                  {activeToolSpec ? activeToolSpec.description : 'System-registered runtime dynamic extension tool.'}
                </p>
              </div>

              {/* Parameters Table Grid */}
              <div className="space-y-3">
                <h2 className="text-xs font-semibold text-zinc-300 flex items-center gap-1.5">
                  <BookOpen size={13} className="text-blue-400" />
                  Parameters Definition
                </h2>
                
                {activeToolSpec && Object.keys(activeToolSpec.parameters.properties).length > 0 ? (
                  <div className="rounded-2xl border border-zinc-900 bg-zinc-900/10 overflow-hidden text-xs">
                    <div className="grid grid-cols-4 bg-zinc-900/40 border-b border-zinc-900 px-4 py-2 text-zinc-500 font-bold">
                      <div className="col-span-1">Field</div>
                      <div className="col-span-1 text-center">Type</div>
                      <div className="col-span-2">Description</div>
                    </div>
                    <div className="divide-y divide-zinc-900">
                      {Object.entries(activeToolSpec.parameters.properties).map(([name, val]) => (
                        <div key={name} className="grid grid-cols-4 px-4 py-3 items-baseline">
                          <div className="col-span-1 font-mono text-[11px] text-zinc-300 font-bold flex items-center gap-1">
                            {name}
                            {val.required && <span className="text-red-500 font-bold" title="Required">*</span>}
                          </div>
                          <div className="col-span-1 text-center">
                            <span className="text-[10px] bg-zinc-900 border border-zinc-800 text-zinc-400 font-semibold px-2 py-0.5 rounded-md font-mono">
                              {val.type}
                            </span>
                          </div>
                          <div className="col-span-2 space-y-1.5">
                            <p className="text-zinc-400 font-normal leading-relaxed">{val.description}</p>
                            {val.enum && (
                              <div className="flex flex-wrap items-center gap-1">
                                <span className="text-[9px] text-zinc-500 font-medium">Enum:</span>
                                {val.enum.map(e => (
                                  <span key={e} className="text-[9px] font-mono text-zinc-400 bg-zinc-900 border border-zinc-800 px-1 rounded-sm">
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
                  <div className="rounded-2xl border border-zinc-900 bg-zinc-900/10 p-5 text-center text-xs text-zinc-500 leading-normal">
                    This tool takes no parameters. Input args are passed as an empty JSON object `{}`.
                  </div>
                )}
              </div>

              {/* Execution Sample Block */}
              {activeToolSpec && (
                <div className="space-y-2.5">
                  <div className="flex items-center justify-between">
                    <h2 className="text-xs font-semibold text-zinc-300 flex items-center gap-1.5">
                      <Terminal size={13} className="text-emerald-400" />
                      Argument JSON Example
                    </h2>
                    <button
                      onClick={() => handleCopyExample(activeToolSpec.example)}
                      className="flex items-center gap-1 px-2.5 py-1.5 rounded-lg border border-zinc-850 hover:bg-zinc-900/60 text-[10px] text-zinc-500 hover:text-zinc-300 transition-colors shrink-0"
                    >
                      {copied ? (
                        <>
                          <Check size={11} className="text-emerald-400" /> Copied
                        </>
                      ) : (
                        <>
                          <Copy size={11} /> Copy JSON
                        </>
                      )}
                    </button>
                  </div>
                  <pre className="bg-zinc-950 border border-zinc-900 p-4 rounded-2xl text-[11px] text-zinc-300 font-mono overflow-x-auto leading-relaxed shadow-sm">
                    {activeToolSpec.example}
                  </pre>
                </div>
              )}
              
            </div>
          ) : (
            <div className="h-full flex items-center justify-center text-xs text-zinc-600">
              Select a tool on the left to inspect its parameters and JSON definitions.
            </div>
          )}
        </div>
      </div>
    </div>
  )
}