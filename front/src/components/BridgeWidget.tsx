'use client'

import { useEffect, useState } from 'react'
import { isAddress, parseEther } from 'viem'
import { useAccount } from 'wagmi'
import { WalletButton } from './WalletButton'
import { AmountInput } from './AmountInput'
import { RecipientInput } from './RecipientInput'
import { BridgeButton } from './BridgeButton'
import { TxStatus } from './TxStatus'
import { useBridge, type Direction } from '@/hooks/useBridge'
import { useNativeBalance } from '@/hooks/useNativeBalance'
import { SEPOLIA_CHAIN_ID, HYLI_CHAIN_ID } from '@/lib/hyperlane'

export function BridgeWidget() {
  const { isConnected } = useAccount()
  const [mounted, setMounted] = useState(false)
  useEffect(() => setMounted(true), [])
  const [amount, setAmount] = useState('')
  const [recipient, setRecipient] = useState('')
  const [direction, setDirection] = useState<Direction>('to_hyli')

  const toHyli = direction === 'to_hyli'
  const fromChain = toHyli ? 'Sepolia' : 'Hyli'
  const toChain   = toHyli ? 'Hyli' : 'Sepolia'
  const sourceChainId = toHyli ? SEPOLIA_CHAIN_ID : HYLI_CHAIN_ID

  const { bridge, reset, status, interchainFee } = useBridge(direction)
  const { balance } = useNativeBalance(sourceChainId)

  let amountWei = 0n
  try { amountWei = amount ? parseEther(amount) : 0n } catch { amountWei = 0n }
  const insufficientFunds = amountWei > 0n && (
    toHyli ? amountWei + interchainFee > balance : amountWei > balance
  )

  const isBusy =
    status.type === 'pending' ||
    status.type === 'switching_chain' ||
    status.type === 'confirming' ||
    status.type === 'relaying'

  function handleSwap() {
    if (isBusy) return
    reset()
    setAmount('')
    setDirection(d => (d === 'to_hyli' ? 'to_sepolia' : 'to_hyli'))
  }

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
            <h1 className="text-xl font-bold text-white">Bridge to {toChain}</h1>
            <p className="text-xs text-gray-500 mt-0.5">{fromChain} &rarr; {toChain} via Hyperlane</p>
          </div>
          <WalletButton />
        </div>

        {/* Route indicator with swap button */}
        <div className="flex items-center justify-between text-sm bg-gray-800 rounded-xl px-4 py-3">
          <div className="text-center">
            <p className="text-gray-400 text-xs">From</p>
            <p className="font-semibold text-white">{fromChain}</p>
          </div>
          <button
            onClick={handleSwap}
            disabled={isBusy}
            className="text-gray-400 hover:text-white disabled:opacity-30 transition-colors p-1 rounded"
            title="Swap direction"
          >
            <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M7 16V4m0 0L3 8m4-4l4 4M17 8v12m0 0l4-4m-4 4l-4-4" />
            </svg>
          </button>
          <div className="text-center">
            <p className="text-gray-400 text-xs">To</p>
            <p className="font-semibold text-white">{toChain}</p>
          </div>
        </div>

        {/* Inputs */}
        <AmountInput value={amount} onChange={setAmount} disabled={isBusy} sourceChainId={sourceChainId} sourceChainName={fromChain} />
        <RecipientInput value={recipient} onChange={setRecipient} disabled={isBusy} destChainName={toChain} />

        {/* Action */}
        {mounted && isConnected ? (
          <BridgeButton amount={amount} status={status} direction={direction} insufficientFunds={insufficientFunds} onBridge={handleBridge} />
        ) : (
          <p className="text-center text-sm text-gray-500">Connect your wallet to bridge</p>
        )}

        {/* Status */}
        <TxStatus status={status} direction={direction} onReset={reset} />
      </div>
    </div>
  )
}
