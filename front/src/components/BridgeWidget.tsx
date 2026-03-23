'use client'

import { useEffect, useState } from 'react'
import { isAddress } from 'viem'
import { useAccount } from 'wagmi'
import { WalletButton } from './WalletButton'
import { AmountInput } from './AmountInput'
import { RecipientInput } from './RecipientInput'
import { BridgeButton } from './BridgeButton'
import { TxStatus } from './TxStatus'
import { useBridge } from '@/hooks/useBridge'

export function BridgeWidget() {
  const { isConnected } = useAccount()
  const [mounted, setMounted] = useState(false)
  useEffect(() => setMounted(true), [])
  const [amount, setAmount] = useState('')
  const [recipient, setRecipient] = useState('')
  const { bridge, reset, status } = useBridge()

  const isBusy = status.type === 'pending' || status.type === 'switching_chain'

  async function handleBridge() {
    if (!amount || parseFloat(amount) <= 0) return

    const recipientAddr =
      recipient && isAddress(recipient) ? (recipient as `0x${string}`) : undefined

    await bridge(amount, recipientAddr)
  }

  return (
    <div className="w-full max-w-md mx-auto">
      <div className="bg-gray-900 border border-gray-700 rounded-2xl p-6 shadow-xl space-y-5">
        {/* Header */}
        <div className="flex items-center justify-between">
          <div>
            <h1 className="text-xl font-bold text-white">Bridge to Hyli</h1>
            <p className="text-xs text-gray-500 mt-0.5">Sepolia → Hyli via Hyperlane</p>
          </div>
          <WalletButton />
        </div>

        {/* Route indicator */}
        <div className="flex items-center justify-between text-sm bg-gray-800 rounded-xl px-4 py-3">
          <div className="text-center">
            <p className="text-gray-400 text-xs">From</p>
            <p className="font-semibold text-white">Sepolia</p>
          </div>
          <svg className="text-gray-500 w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 8l4 4m0 0l-4 4m4-4H3" />
          </svg>
          <div className="text-center">
            <p className="text-gray-400 text-xs">To</p>
            <p className="font-semibold text-white">Hyli</p>
          </div>
        </div>

        {/* Inputs */}
        <AmountInput value={amount} onChange={setAmount} disabled={isBusy} />
        <RecipientInput value={recipient} onChange={setRecipient} disabled={isBusy} />

        {/* Action */}
        {mounted && isConnected ? (
          <BridgeButton amount={amount} status={status} onBridge={handleBridge} />
        ) : (
          <p className="text-center text-sm text-gray-500">Connect your wallet to bridge</p>
        )}

        {/* Status */}
        <TxStatus status={status} onReset={reset} />
      </div>
    </div>
  )
}
