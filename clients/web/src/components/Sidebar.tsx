import { useState, useEffect } from 'react'
import { NavLink } from 'react-router-dom'
import {
  MessageSquare, Layers, Wrench, Sparkles, Brain, Settings,
  PanelLeftClose, PanelLeftOpen, Menu, X, LogOut,
} from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { AUTH_TOKEN_KEY } from '../hooks/useWebSocket'

const links = [
  { to: '/', icon: MessageSquare, label: 'Chat' },
  { to: '/sessions', icon: Layers, label: 'Sessions' },
  { to: '/tools', icon: Wrench, label: 'Tools' },
  { to: '/skills', icon: Sparkles, label: 'Skills' },
  { to: '/memory', icon: Brain, label: 'Memory' },
  { to: '/config', icon: Settings, label: 'Config' },
]

function useIsMobile() {
  const [isMobile, setIsMobile] = useState(() => window.innerWidth < 768)
  useEffect(() => {
    const handler = () => setIsMobile(window.innerWidth < 768)
    window.addEventListener('resize', handler)
    return () => window.removeEventListener('resize', handler)
  }, [])
  return isMobile
}

function NavLinkItem({ to, icon: Icon, label, collapsed, onNavigate }: {
  to: string; icon: React.ComponentType<any>; label: string; collapsed: boolean; onNavigate?: () => void
}) {
  return (
    <NavLink
      to={to}
      end={to === '/'}
      title={collapsed ? label : undefined}
      onClick={(e) => {
        if ((window as any).myclawUnsaved) {
          if (!window.confirm('You have unsaved changes. Are you sure you want to leave this page?')) {
            e.preventDefault(); return
          }
        }
        onNavigate?.()
      }}
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
  )
}

// ── Desktop Sidebar (collapsible) ────────────────────────────────────────

function DesktopSidebar({ collapsed, setCollapsed }: { collapsed: boolean; setCollapsed: (v: boolean) => void }) {
  const { status } = useWebSocketContext()

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

  const handleLogout = () => {
    if (!window.confirm('Log out? You will need to re-enter your access token.')) return
    localStorage.removeItem(AUTH_TOKEN_KEY)
    window.location.reload()
  }

  return (
    <aside className={`${collapsed ? 'w-14' : 'w-56'} hidden md:flex flex-col shrink-0 bg-zinc-900 border-r border-zinc-800 transition-[width] duration-200 overflow-hidden`}>
      <div className="flex items-center h-14 px-3 border-b border-zinc-800 shrink-0">
        {!collapsed && <span className="flex-1 text-sm font-semibold text-zinc-100 truncate">🦀 MyClaw</span>}
        <button
          onClick={() => setCollapsed(!collapsed)}
          className={`p-1.5 rounded-lg text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800 transition ${collapsed ? 'mx-auto' : ''}`}
          title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
        >
          {collapsed ? <PanelLeftOpen size={16} /> : <PanelLeftClose size={16} />}
        </button>
      </div>
      <nav className="flex-1 px-2 py-2 space-y-0.5 overflow-y-auto">
        {links.map(({ to, icon, label }) => (
          <NavLinkItem key={to} to={to} icon={icon} label={label} collapsed={collapsed} />
        ))}
      </nav>
      <div className={`flex items-center gap-2.5 px-4 py-3.5 border-t border-zinc-800 shrink-0 ${collapsed ? 'justify-center' : ''}`}>
        <span className={`h-1.5 w-1.5 rounded-full shrink-0 ${statusBgColor} ${statusShadow}`} />
        {!collapsed && <span className={`text-[10px] font-semibold tracking-wide truncate uppercase ${statusColor}`}>{statusText}</span>}
        {!collapsed && (
          <button onClick={handleLogout} title="Log out" className="ml-auto p-1 rounded-md text-zinc-600 hover:text-red-400 hover:bg-zinc-800 transition-colors">
            <LogOut size={13} />
          </button>
        )}
      </div>
    </aside>
  )
}

// ── Mobile Drawer ────────────────────────────────────────────────────────

function MobileDrawer({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { status } = useWebSocketContext()

  const statusColor =
    status === 'connected' ? 'text-emerald-400' :
    status === 'connecting' ? 'text-amber-400' : 'text-red-400'
  const statusText =
    status === 'connected' ? 'Connected' :
    status === 'connecting' ? 'Connecting…' : 'Disconnected'
  const statusBgColor =
    status === 'connected' ? 'bg-emerald-400' :
    status === 'connecting' ? 'bg-amber-400' : 'bg-red-400'

  // Prevent body scroll when open
  useEffect(() => {
    if (open) {
      document.body.style.overflow = 'hidden'
      return () => { document.body.style.overflow = '' }
    }
  }, [open])

  if (!open) return null

  return (
    <div className="fixed inset-0 z-40 md:hidden">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/50 backdrop-blur-[2px]" onClick={onClose} />
      {/* Panel */}
      <div className="absolute inset-y-0 left-0 w-64 bg-zinc-900 border-r border-zinc-800 flex flex-col animate-[slideIn_0.15s_ease-out]">
        <style>{`@keyframes slideIn { from { transform: translateX(-100%); } to { transform: translateX(0); } }`}</style>
        <div className="flex items-center h-14 px-4 border-b border-zinc-800 shrink-0">
          <span className="flex-1 text-sm font-semibold text-zinc-100">🦀 MyClaw</span>
          <button onClick={onClose} className="p-1.5 rounded-lg text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800 transition">
            <X size={18} />
          </button>
        </div>
        <nav className="flex-1 px-2 py-3 space-y-1 overflow-y-auto">
          {links.map(({ to, icon, label }) => (
            <NavLinkItem key={to} to={to} icon={icon} label={label} collapsed={false} onNavigate={onClose} />
          ))}
        </nav>
        <div className="flex items-center gap-2.5 px-4 py-3.5 border-t border-zinc-800">
          <span className={`h-2 w-2 rounded-full ${statusBgColor}`} />
          <span className={`text-[11px] font-semibold tracking-wide uppercase ${statusColor}`}>{statusText}</span>
          <button onClick={() => { onClose(); localStorage.removeItem(AUTH_TOKEN_KEY); window.location.reload() }} title="Log out" className="ml-auto p-1.5 rounded-md text-zinc-600 hover:text-red-400 hover:bg-zinc-800 transition-colors">
            <LogOut size={14} />
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Main Sidebar Component ───────────────────────────────────────────────

export default function Sidebar() {
  const isMobile = useIsMobile()
  const [collapsed, setCollapsed] = useState(false)
  const [drawerOpen, setDrawerOpen] = useState(false)

  // Close drawer when switching to desktop
  useEffect(() => {
    if (!isMobile) setDrawerOpen(false)
  }, [isMobile])

  return (
    <>
      {/* Mobile top bar (replaces sidebar) */}
      {isMobile && (
        <div className="fixed top-0 left-0 right-0 h-12 bg-zinc-900 border-b border-zinc-800 flex items-center px-3 z-30 md:hidden">
          <button
            onClick={() => setDrawerOpen(true)}
            className="p-1.5 rounded-lg text-zinc-400 hover:text-zinc-200 hover:bg-zinc-800 transition"
          >
            <Menu size={20} />
          </button>
          <span className="ml-2 text-sm font-semibold text-zinc-100">🦀 MyClaw</span>
        </div>
      )}

      {/* Desktop sidebar */}
      <DesktopSidebar collapsed={collapsed} setCollapsed={setCollapsed} />

      {/* Mobile drawer */}
      <MobileDrawer open={drawerOpen} onClose={() => setDrawerOpen(false)} />
    </>
  )
}
