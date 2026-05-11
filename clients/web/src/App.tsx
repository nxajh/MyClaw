import { Routes, Route, Navigate } from 'react-router-dom'
import { WebSocketProvider } from './contexts/WebSocketContext'
import Sidebar from './components/Sidebar'
import Chat from './pages/Chat'
import Sessions from './pages/Sessions'
import Placeholder from './pages/Placeholder'

export default function App() {
  return (
    <WebSocketProvider>
      <div className="flex h-screen w-screen overflow-hidden bg-zinc-900 text-zinc-100">
        <Sidebar />
        <main className="flex-1 flex flex-col min-w-0">
          <Routes>
            <Route path="/" element={<Chat />} />
            <Route path="/sessions" element={<Sessions />} />
            <Route path="/tools" element={<Placeholder title="Tools" />} />
            <Route path="/memory" element={<Placeholder title="Memory" />} />
            <Route path="/config" element={<Placeholder title="Config" />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </main>
      </div>
    </WebSocketProvider>
  )
}
