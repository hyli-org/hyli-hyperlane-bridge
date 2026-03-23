'use client'

import { useEffect, useRef, useState } from 'react'
import { useAccount, useConnect, useDisconnect, useConnectors } from 'wagmi'
import { injected } from 'wagmi/connectors'

export function WalletButton() {
  const { address, isConnected } = useAccount()
  const { connect } = useConnect()
  const { disconnect } = useDisconnect()
  const connectors = useConnectors()
  const [mounted, setMounted] = useState(false)
  const [open, setOpen] = useState(false)
  const dialogRef = useRef<HTMLDivElement>(null)

  useEffect(() => setMounted(true), [])

  // Close on outside click
  useEffect(() => {
    if (!open) return
    function handleClick(e: MouseEvent) {
      if (dialogRef.current && !dialogRef.current.contains(e.target as Node)) {
        setOpen(false)
      }
    }
    document.addEventListener('mousedown', handleClick)
    return () => document.removeEventListener('mousedown', handleClick)
  }, [open])

  if (!mounted) return null

  if (isConnected && address) {
    return (
      <div className="flex items-center gap-2">
        <span className="text-xs text-gray-400 font-mono bg-gray-800 px-2.5 py-1.5 rounded-lg border border-gray-700">
          {address.slice(0, 6)}…{address.slice(-4)}
        </span>
        <button
          onClick={() => disconnect()}
          className="text-xs px-2.5 py-1.5 rounded-lg border border-gray-600 text-gray-400 hover:text-white hover:border-gray-400 transition-colors"
        >
          Disconnect
        </button>
      </div>
    )
  }

  const eip6963Connectors = connectors.filter((c) => c.type === 'injected' || c.id !== 'injected')
  const hasConnectors = eip6963Connectors.length > 0

  return (
    <div className="relative">
      <button
        onClick={() => setOpen(true)}
        className="px-3 py-1.5 text-sm rounded-lg bg-blue-600 hover:bg-blue-500 text-white font-medium transition-colors"
      >
        Connect
      </button>

      {open && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4">
          {/* Backdrop */}
          <div className="absolute inset-0 bg-black/60 backdrop-blur-sm" />

          {/* Modal */}
          <div
            ref={dialogRef}
            className="relative z-10 w-full max-w-sm bg-gray-900 border border-gray-700 rounded-2xl shadow-2xl overflow-hidden"
          >
            {/* Header */}
            <div className="flex items-center justify-between px-5 py-4 border-b border-gray-800">
              <h2 className="text-base font-semibold text-white">Connect a wallet</h2>
              <button
                onClick={() => setOpen(false)}
                className="text-gray-500 hover:text-white transition-colors rounded-lg p-1 hover:bg-gray-800"
              >
                <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                  <path strokeLinecap="round" strokeLinejoin="round" d="M6 18L18 6M6 6l12 12" />
                </svg>
              </button>
            </div>

            {/* Wallet list */}
            <div className="p-3 space-y-1.5">
              {hasConnectors ? (
                eip6963Connectors.map((connector) => (
                  <button
                    key={connector.uid}
                    onClick={() => { connect({ connector }); setOpen(false) }}
                    className="w-full flex items-center gap-3 px-4 py-3 rounded-xl hover:bg-gray-800 transition-colors group text-left"
                  >
                    {connector.icon ? (
                      // eslint-disable-next-line @next/next/no-img-element
                      <img src={connector.icon} alt="" className="w-8 h-8 rounded-lg flex-shrink-0" />
                    ) : (
                      <div className="w-8 h-8 rounded-lg bg-gray-700 flex items-center justify-center flex-shrink-0">
                        <svg className="w-4 h-4 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                          <path strokeLinecap="round" strokeLinejoin="round" d="M3 10h18M7 15h1m4 0h1m-7 4h12a3 3 0 003-3V8a3 3 0 00-3-3H6a3 3 0 00-3 3v8a3 3 0 003 3z" />
                        </svg>
                      </div>
                    )}
                    <span className="text-sm font-medium text-gray-200 group-hover:text-white transition-colors">
                      {connector.name}
                    </span>
                    <svg className="w-4 h-4 text-gray-600 group-hover:text-gray-400 ml-auto transition-colors" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                      <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
                    </svg>
                  </button>
                ))
              ) : (
                <button
                  onClick={() => { connect({ connector: injected() }); setOpen(false) }}
                  className="w-full flex items-center gap-3 px-4 py-3 rounded-xl hover:bg-gray-800 transition-colors group text-left"
                >
                  <div className="w-8 h-8 rounded-lg bg-gray-700 flex items-center justify-center flex-shrink-0">
                    <svg className="w-4 h-4 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                      <path strokeLinecap="round" strokeLinejoin="round" d="M3 10h18M7 15h1m4 0h1m-7 4h12a3 3 0 003-3V8a3 3 0 00-3-3H6a3 3 0 00-3 3v8a3 3 0 003 3z" />
                    </svg>
                  </div>
                  <span className="text-sm font-medium text-gray-200 group-hover:text-white transition-colors">
                    Browser Wallet
                  </span>
                  <svg className="w-4 h-4 text-gray-600 group-hover:text-gray-400 ml-auto transition-colors" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
                    <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
                  </svg>
                </button>
              )}
            </div>

            <p className="text-center text-xs text-gray-600 pb-4">
              By connecting you agree to the terms of service
            </p>
          </div>
        </div>
      )}
    </div>
  )
}
