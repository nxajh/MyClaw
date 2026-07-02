import { lazy, Suspense, useState, useEffect, useRef } from 'react'
import { Routes, Route, Navigate } from 'react-router-dom'
import { WebSocketProvider } from './contexts/WebSocketContext'
import { useWebSocketContext } from './contexts/WebSocketContext'
import { ToastProvider, useToast } from './components/Toast'
import Sidebar from './components/Sidebar'
import LoginOverlay from './components/LoginOverlay'
import CommandPalette from './components/CommandPalette'
import { AUTH_TOKEN_KEY } from './hooks/useWebSocket'

const Chat = lazy(() => import('./pages/Chat'))
const Sessions = lazy(() => import('./pages/Sessions'))
const Tools = lazy(() => import('./pages/Tools'))
const Skills = lazy(() => import('./pages/Skills'))
const Memory = lazy(() => import('./pages/Memory'))
const Config = lazy(() => import('./pages/Config'))
const Overview = lazy(() => import('./pages/Overview'))

function PageLoader() {
  return (
    <div className="flex-1 flex items-center justify-center">
      <div className="h-5 w-5 border-2 border-zinc-700 border-t-zinc-300 rounded-full animate-spin" />
    </div>
  )
}

function useIsMobile() {
  const [isMobile, setIsMobile] = useState(() => window.innerWidth < 768)
  useEffect(() => {
    const handler = () => setIsMobile(window.innerWidth < 768)
    window.addEventListener('resize', handler)
    return () => window.removeEventListener('resize', handler)
  }, [])
  return isMobile
}

function DisconnectBanner() {
  const { status, reconnectNow } = useWebSocketContext()
  const { toast } = useToast()
  const prev = useRef(status)

  useEffect(() => {
    if (prev.current !== 'connected' && status === 'connected') toast('Connection restored', 'success')
    prev.current = status
  }, [status, toast])

  if (status === 'connected') return null
  return (
    <div className="shrink-0 border-b border-zinc-800 bg-zinc-900/80 px-3 sm:px-4 py-1.5 text-xs text-zinc-400 flex items-center justify-center gap-2 shadow-sm">
      <span className={`h-1.5 w-1.5 rounded-full ${status === 'connecting' ? 'bg-zinc-400 animate-pulse' : 'bg-zinc-500'}`} />
      <span>{status === 'connecting' ? 'Reconnecting…' : 'Disconnected — messages may not be delivered'}</span>
      <button onClick={reconnectNow} className="ml-1 px-2 py-0.5 rounded-md border border-zinc-700/60 hover:bg-zinc-800 text-zinc-300 hover:text-zinc-100 transition-colors">
        Reconnect now
      </button>
    </div>
  )
}

function AppShell() {
  const { authFailed, authValidating, submitToken, status } = useWebSocketContext()
  const isMobile = useIsMobile()

  const hasStoredToken = !!localStorage.getItem(AUTH_TOKEN_KEY)
  const showLogin = authFailed || authValidating || (!hasStoredToken && status !== 'connected')

  return (
    <div className="flex h-[100dvh] w-screen overflow-hidden bg-zinc-950 text-zinc-100">
      {showLogin && (
        <LoginOverlay onSubmit={submitToken} isRetry={authFailed} isConnecting={authValidating} />
      )}
      <CommandPalette />
      <Sidebar />
      <main className={`flex-1 flex flex-col min-w-0 min-h-0 ${isMobile ? 'pt-12' : ''}`}>
        <DisconnectBanner />
        <Suspense fallback={<PageLoader />}>
          <Routes>
            <Route path="/" element={<Chat />} />
            <Route path="/overview" element={<Overview />} />
            <Route path="/sessions" element={<Sessions />} />
            <Route path="/tools" element={<Tools />} />
            <Route path="/skills" element={<Skills />} />
            <Route path="/memory" element={<Memory />} />
            <Route path="/config" element={<Config />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </Suspense>
      </main>
    </div>
  )
}

export default function App() {
  return (
    <ToastProvider>
      <WebSocketProvider>
        <AppShell />
      </WebSocketProvider>
    </ToastProvider>
  )
}
