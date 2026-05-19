import { useState } from 'react'
import { NavLink } from 'react-router-dom'
import {
  MessageSquare, Layers, Wrench, Sparkles, Brain, Settings,
  Wifi, WifiOff, Loader, PanelLeftClose, PanelLeftOpen,
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

  const StatusIcon = status === 'connected' ? Wifi : status === 'connecting' ? Loader : WifiOff
  const statusColor =
    status === 'connected' ? 'text-emerald-400' :
    status === 'connecting' ? 'text-amber-400 animate-pulse' : 'text-red-400'
  const statusText =
    status === 'connected' ? 'Connected' :
    status === 'connecting' ? 'Connecting…' : 'Disconnected'

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
              `flex items-center gap-3 px-2.5 py-2 rounded-lg text-sm transition-colors ${
                collapsed ? 'justify-center' : ''
              } ${
                isActive
                  ? 'bg-zinc-800 text-zinc-100'
                  : 'text-zinc-400 hover:bg-zinc-800/60 hover:text-zinc-200'
              }`
            }
          >
            <Icon size={17} className="shrink-0" />
            {!collapsed && <span className="truncate">{label}</span>}
          </NavLink>
        ))}
      </nav>

      {/* Status */}
      <div
        className={`flex items-center gap-2 px-3 py-3 border-t border-zinc-800 shrink-0 ${
          collapsed ? 'justify-center' : ''
        }`}
        title={collapsed ? statusText : undefined}
      >
        <StatusIcon size={13} className={`shrink-0 ${statusColor}`} />
        {!collapsed && <span className={`text-xs truncate ${statusColor}`}>{statusText}</span>}
      </div>
    </aside>
  )
}
