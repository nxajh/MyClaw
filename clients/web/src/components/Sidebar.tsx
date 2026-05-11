import { NavLink } from 'react-router-dom'
import { MessageSquare, Layers, Wrench, Brain, Settings, Wifi, WifiOff, Loader } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'

const links = [
  { to: '/', icon: MessageSquare, label: 'Chat' },
  { to: '/sessions', icon: Layers, label: 'Sessions' },
  { to: '/tools', icon: Wrench, label: 'Tools' },
  { to: '/memory', icon: Brain, label: 'Memory' },
  { to: '/config', icon: Settings, label: 'Config' },
]

export default function Sidebar() {
  const { status } = useWebSocketContext()

  const StatusIcon = status === 'connected' ? Wifi : status === 'connecting' ? Loader : WifiOff
  const statusColor =
    status === 'connected'
      ? 'text-emerald-400'
      : status === 'connecting'
        ? 'text-amber-400 animate-pulse'
        : 'text-red-400'
  const statusText =
    status === 'connected' ? 'Connected' : status === 'connecting' ? 'Connecting…' : 'Disconnected'

  return (
    <aside className="w-60 flex flex-col border-r border-zinc-700/50 bg-zinc-900 shrink-0">
      {/* Logo */}
      <div className="px-4 py-5 border-b border-zinc-700/50">
        <h1 className="text-lg font-bold tracking-tight text-zinc-100">🦀 MyClaw</h1>
      </div>

      {/* Nav links */}
      <nav className="flex-1 px-2 py-4 space-y-1">
        {links.map(({ to, icon: Icon, label }) => (
          <NavLink
            key={to}
            to={to}
            end={to === '/'}
            className={({ isActive }) =>
              `flex items-center gap-3 px-3 py-2 rounded-lg text-sm transition-colors ${
                isActive
                  ? 'bg-zinc-700/60 text-zinc-100 font-medium'
                  : 'text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200'
              }`
            }
          >
            <Icon size={18} />
            {label}
          </NavLink>
        ))}
      </nav>

      {/* Connection status */}
      <div className="px-4 py-3 border-t border-zinc-700/50 flex items-center gap-2 text-xs">
        <StatusIcon size={14} className={statusColor} />
        <span className={statusColor}>{statusText}</span>
      </div>
    </aside>
  )
}
