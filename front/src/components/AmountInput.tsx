'use client'

import { useEffect, useState } from 'react'
import { useNativeBalance } from '@/hooks/useNativeBalance'
import { useAccount } from 'wagmi'

interface AmountInputProps {
  value: string
  onChange: (value: string) => void
  disabled?: boolean
}

export function AmountInput({ value, onChange, disabled }: AmountInputProps) {
  const { isConnected } = useAccount()
  const { formatted, symbol } = useNativeBalance()
  const [mounted, setMounted] = useState(false)
  useEffect(() => setMounted(true), [])

  function handleMax() {
    // Leave a small buffer for gas
    const bal = parseFloat(formatted)
    if (bal > 0.001) {
      onChange((bal - 0.001).toFixed(6))
    }
  }

  return (
    <div className="space-y-1.5">
      <div className="flex justify-between items-center">
        <label className="text-sm font-medium text-gray-300">Amount</label>
        {mounted && isConnected && (
          <span className="text-xs text-gray-500">
            Balance:{' '}
            <button
              onClick={handleMax}
              className="text-blue-400 hover:text-blue-300 underline"
              disabled={disabled}
            >
              {parseFloat(formatted).toFixed(4)} {symbol}
            </button>
          </span>
        )}
      </div>
      <div className="relative flex items-center">
        <input
          type="number"
          min="0"
          step="0.001"
          placeholder="0.0"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          disabled={disabled}
          className="w-full bg-gray-800 border border-gray-600 rounded-lg px-4 py-3 text-white placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent disabled:opacity-50 pr-14"
        />
        <span className="absolute right-4 text-gray-400 text-sm font-medium">ETH</span>
      </div>
    </div>
  )
}
