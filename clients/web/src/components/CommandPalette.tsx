import { useEffect, useState, useRef, useMemo } from 'react'
import { useNavigate } from 'react-router-dom'
import { Terminal, MessageSquare, Compass, BookOpen, Settings, Plus, Search, FolderHeart } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface CommandItem {
  id: string
  title: string
  subtitle: string
  icon: React.ComponentType<any>
  category: string
  action: () => void
  shortcut?: string
}

export default function CommandPalette() {
  const [isOpen, setIsOpen] = useState(false)
  const [search, setSearch] = useState('')
  const [selectedIndex, setSelectedIndex] = useState(0)
  
  const navigate = useNavigate()
  const { status, sendRaw, addMessageListener, reloadHistory, triggerClearInput, setMessages } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  
  const inputRef = useRef<HTMLInputElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)

  // Toggle Command Palette with Cmd+K / Ctrl+K, 'N' for new chat
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault()
        setIsOpen(prev => !prev)
      } else if (e.key === 'Escape') {
        setIsOpen(false)
      } else if (e.key === 'n' && !e.metaKey && !e.ctrlKey && !isOpen) {
        // Only trigger when not typing in an input and palette is closed
        const tag = (e.target as HTMLElement)?.tagName
        if (tag !== 'INPUT' && tag !== 'TEXTAREA' && !(e.target as HTMLElement)?.isContentEditable) {
          e.preventDefault()
          handleNewSession()
        }
      }
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [isOpen])

  // Auto-focus search input when opened
  useEffect(() => {
    if (isOpen) {
      setSearch('')
      setSelectedIndex(0)
      setTimeout(() => {
        inputRef.current?.focus()
      }, 50)
    }
  }, [isOpen])

  // Close palette when clicking outside the panel
  useEffect(() => {
    if (!isOpen) return
    const handleClickOutside = (e: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setIsOpen(false)
      }
    }
    document.addEventListener('mousedown', handleClickOutside)
    return () => document.removeEventListener('mousedown', handleClickOutside)
  }, [isOpen])

  const handleNewSession = async () => {
    if (status !== 'connected') return
    try {
      await request('sessions.create', { name: `Chat ${new Date().toLocaleString()}` })
      triggerClearInput()
      setMessages([])
      await reloadHistory()
      navigate('/')
    } catch (err) {
      console.error('Failed to create session via palette:', err)
    }
  }

  // All executable items in Command Palette
  const commands = useMemo<CommandItem[]>(() => {
    return [
      {
        id: 'new_chat',
        title: 'New Chat Session',
        subtitle: 'Instantly create a fresh, clean chat session',
        icon: Plus,
        category: 'Actions',
        action: () => { handleNewSession(); setIsOpen(false); },
        shortcut: 'N'
      },
      {
        id: 'go_chat',
        title: 'Jump to Active Chat',
        subtitle: 'Open the main chat workspace',
        icon: MessageSquare,
        category: 'Navigation',
        action: () => { navigate('/'); setIsOpen(false); }
      },
      {
        id: 'go_memory',
        title: 'Manage Semantic Memory',
        subtitle: 'View and align preferences & correction facts',
        icon: FolderHeart,
        category: 'Navigation',
        action: () => { navigate('/memory'); setIsOpen(false); }
      },
      {
        id: 'go_skills',
        title: 'Configure Skills Library',
        subtitle: 'Examine custom behaviors and template assets',
        icon: BookOpen,
        category: 'Navigation',
        action: () => { navigate('/skills'); setIsOpen(false); }
      },
      {
        id: 'go_tools',
        title: 'Inspect Tools Integration',
        subtitle: 'Browse all active builtin tool schemas',
        icon: Compass,
        category: 'Navigation',
        action: () => { navigate('/tools'); setIsOpen(false); }
      },
      {
        id: 'go_config',
        title: 'System Settings',
        subtitle: 'Tweak knowledge base and listen ports',
        icon: Settings,
        category: 'Navigation',
        action: () => { navigate('/config'); setIsOpen(false); }
      }
    ]
  }, [navigate, status])

  // Filter commands by search term
  const filteredCommands = useMemo(() => {
    if (!search.trim()) return commands
    return commands.filter(item => 
      item.title.toLowerCase().includes(search.toLowerCase()) ||
      item.subtitle.toLowerCase().includes(search.toLowerCase()) ||
      item.category.toLowerCase().includes(search.toLowerCase())
    )
  }, [search, commands])

  // Update selected index to ensure it is in bounds
  useEffect(() => {
    setSelectedIndex(0)
  }, [filteredCommands])

  // Key navigation logic
  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (filteredCommands.length === 0) return

    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSelectedIndex(prev => (prev + 1) % filteredCommands.length)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSelectedIndex(prev => (prev - 1 + filteredCommands.length) % filteredCommands.length)
    } else if (e.key === 'Enter') {
      e.preventDefault()
      filteredCommands[selectedIndex].action()
    }
  }

  if (!isOpen) return null

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh] sm:pt-[15vh] px-3 sm:px-4 bg-black/40 backdrop-blur-[2px]">
      <div 
        ref={containerRef}
        onKeyDown={handleKeyDown}
        className="w-full max-w-lg bg-zinc-950/95 border border-zinc-800 shadow-2xl rounded-2xl overflow-hidden flex flex-col max-h-[60vh] sm:max-h-[50vh] transition-all duration-200"
      >
        {/* Search header input */}
        <div className="relative border-b border-zinc-900 flex items-center shrink-0">
          <Search size={16} className="absolute left-4 text-zinc-500 pointer-events-none" />
          <input
            ref={inputRef}
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Type a command or jump to page..."
            className="w-full bg-transparent pl-11 pr-4 py-3.5 text-xs text-zinc-100 placeholder-zinc-600 outline-none"
          />
          <div className="absolute right-4 text-[10px] text-zinc-600 bg-zinc-900/60 border border-zinc-800/80 px-1.5 py-0.5 rounded-md font-mono">
            ESC
          </div>
        </div>

        {/* Results Body */}
        <div className="flex-1 overflow-y-auto p-1.5 space-y-1">
          {filteredCommands.length === 0 ? (
            <div className="text-center py-8 text-xs text-zinc-600 flex flex-col items-center gap-1">
              <Terminal size={18} className="text-zinc-700 animate-pulse" />
              <span>No commands found matching your search</span>
            </div>
          ) : (
            filteredCommands.map((cmd, idx) => {
              const Icon = cmd.icon
              const isSelected = idx === selectedIndex
              
              return (
                <button
                  key={cmd.id}
                  onClick={() => cmd.action()}
                  onMouseEnter={() => setSelectedIndex(idx)}
                  className={`w-full text-left rounded-xl px-3 py-2.5 flex items-center gap-3 transition-colors ${
                    isSelected ? 'bg-zinc-900 text-zinc-100' : 'hover:bg-zinc-900/40 text-zinc-400'
                  }`}
                >
                  <div className={`p-2 rounded-lg shrink-0 ${isSelected ? 'bg-zinc-800 text-blue-400' : 'bg-zinc-900/30 text-zinc-600'}`}>
                    <Icon size={14} />
                  </div>
                  
                  <div className="flex-1 min-w-0">
                    <div className={`text-xs font-semibold ${isSelected ? 'text-zinc-200' : 'text-zinc-400'}`}>
                      {cmd.title}
                    </div>
                    <div className="text-[10px] text-zinc-500 truncate mt-0.5">
                      {cmd.subtitle}
                    </div>
                  </div>

                  {cmd.shortcut && (
                    <div className="shrink-0 text-[9px] bg-zinc-950 border border-zinc-800 px-1.5 py-0.5 rounded-md font-mono text-zinc-600">
                      {cmd.shortcut}
                    </div>
                  )}
                </button>
              )
            })
          )}
        </div>

        {/* Shortcut Legend Footer */}
        <div className="px-4 py-2 border-t border-zinc-900 bg-zinc-950 shrink-0 flex items-center justify-between text-[9px] text-zinc-600 font-medium">
          <div className="flex items-center gap-3">
            <span>↑↓ Navigation</span>
            <span>↵ Select</span>
          </div>
          <span>Cmd/Ctrl+K to close</span>
        </div>
      </div>
    </div>
  )
}