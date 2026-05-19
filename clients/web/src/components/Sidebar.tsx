import { useState } from 'react'
import { NavLink } from 'react-router-dom'
import {
  MessageSquare, Layers, Wrench, Sparkles, Brain, Settings,
  PanelLeftClose, PanelLeftOpen,
} from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'

const links = [
  { to: '/', icon: MessageSquare, label: 'Chat' },
  { to: '/sessions', icon: Layers, label: 'Sessions' },
  { to: '/tools', icon: Wrench, label: 'Tools' },
  { to: '/skills', icon: Sparkles, label: 'Skills' },
  { to: '/memory', icon: Brain, label: 'Memory' },
  { to: '/config', icon: Settings, label: 'Config' },
]

export default function Sidebar() {
  const { status } = useWebSocketContext()
  const [collapsed, setCollapsed] = useState(false)

  const statusColor =
    status === 'connected' ? 'text-emerald-400' :
    status === 'connecting' ? 'text-amber-400 animate-pulse' : 'text-red-400'
  const statusText =
    status === 'connected' ? 'Connected' :
    status === 'connecting' ? 'Connecting…' : 'Disconnected'

  const statusBgColor =
    status === 'connected' ? 'bg-emerald-400' :
    status === 'connecting' ? 'bg-amber-400' : 'bg-red-400'

  const statusShadow =
    status === 'connected' ? 'shadow-[0_0_8px_rgba(52,211,153,0.6)]' :
    status === 'connecting' ? 'shadow-[0_0_8px_rgba(251,191,36,0.6)] animate-pulse' :
    'shadow-[0_0_8px_rgba(248,113,113,0.6)]'

  return (
    <aside
      className={`${collapsed ? 'w-14' : 'w-56'} flex flex-col shrink-0 bg-zinc-900 border-r border-zinc-800 transition-[width] duration-200 overflow-hidden`}
    >
      {/* Header */}
      <div className="flex items-center h-14 px-3 border-b border-zinc-800 shrink-0">
        {!collapsed && (
          <span className="flex-1 text-sm font-semibold text-zinc-100 truncate">🦀 MyClaw</span>
        )}
        <button
          onClick={() => setCollapsed(!collapsed)}
          className={`p-1.5 rounded-lg text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800 transition ${collapsed ? 'mx-auto' : ''}`}
          title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
        >
          {collapsed ? <PanelLeftOpen size={16} /> : <PanelLeftClose size={16} />}
        </button>
      </div>

      {/* Nav */}
      <nav className="flex-1 px-2 py-2 space-y-0.5 overflow-y-auto">
        {links.map(({ to, icon: Icon, label }) => (
          <NavLink
            key={to}
            to={to}
            end={to === '/'}
            title={collapsed ? label : undefined}
            className={({ isActive }) =>
              `flex items-center gap-3 px-2.5 py-2 rounded-lg text-sm transition-all duration-150 relative ${
                collapsed ? 'justify-center' : ''
              } ${
                isActive
                  ? 'bg-zinc-800/80 text-zinc-100 shadow-sm border border-zinc-700/20'
                  : 'text-zinc-400 hover:bg-zinc-800/40 hover:text-zinc-200'
              }`
            }
          >
            {({ isActive }) => (
              <>
                {isActive && !collapsed && (
                  <span className="absolute left-0 top-2 bottom-2 w-1 rounded-r-md bg-blue-500" />
                )}
                <Icon size={17} className="shrink-0" />
                {!collapsed && <span className="truncate">{label}</span>}
              </>
            )}
          </NavLink>
        ))}
      </nav>

      {/* Status */}
      <div
        className={`flex items-center gap-2.5 px-4 py-3.5 border-t border-zinc-800 shrink-0 ${
          collapsed ? 'justify-center' : ''
        }`}
        title={collapsed ? statusText : undefined}
      >
        <span className={`h-1.5 w-1.5 rounded-full shrink-0 ${statusBgColor} ${statusShadow}`} />
        {!collapsed && <span className={`text-[10px] font-semibold tracking-wide truncate uppercase ${statusColor}`}>{statusText}</span>}
      </div>
    </aside>
  )
}
