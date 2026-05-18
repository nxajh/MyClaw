import { Routes, Route, Navigate } from 'react-router-dom'
import { WebSocketProvider } from './contexts/WebSocketContext'
import { useWebSocketContext } from './contexts/WebSocketContext'
import Sidebar from './components/Sidebar'
import LoginOverlay from './components/LoginOverlay'
import Chat from './pages/Chat'
import Sessions from './pages/Sessions'
import Tools from './pages/Tools'
import Memory from './pages/Memory'
import Config from './pages/Config'
import { AUTH_TOKEN_KEY } from './hooks/useWebSocket'

function AppShell() {
  const { authFailed, submitToken, status } = useWebSocketContext()

  // Show login overlay when auth was rejected, or when there is no stored
  // token and we haven't connected yet (first-time / unauthenticated visitor).
  const hasStoredToken = !!localStorage.getItem(AUTH_TOKEN_KEY)
  const showLogin = authFailed || (!hasStoredToken && status !== 'connected')

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-zinc-950 text-zinc-100">
      {showLogin && (
        <LoginOverlay onSubmit={submitToken} isRetry={authFailed} />
      )}
      <Sidebar />
      <main className="flex-1 flex flex-col min-w-0">
        <Routes>
          <Route path="/" element={<Chat />} />
          <Route path="/sessions" element={<Sessions />} />
          <Route path="/tools" element={<Tools />} />
          <Route path="/memory" element={<Memory />} />
          <Route path="/config" element={<Config />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </div>
  )
}

export default function App() {
  return (
    <WebSocketProvider>
      <AppShell />
    </WebSocketProvider>
  )
}
